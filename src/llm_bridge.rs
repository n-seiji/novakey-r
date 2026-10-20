//! AI 並べ替えの実行部分。判断は `rerank::State`、ここは Keychain (security-framework)・HTTPS (ureq + rustls)・
//! ワーカースレッド・メインスレッドへの結果配信を受け持つ。unsafe は持たない (メインキューへの投入は `platform`)。
//!
//! 契約: 結果ハンドラはメインスレッドで呼ばれる。認証情報・トークン・リクエスト本文はログに出さない。
#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Instant;

use crate::platform;
use crate::rerank::{
    self, CredentialStore, Credentials, Delivery, HttpRequest, HttpResponse, InFlight, Job, State, Transport,
};

pub use crate::rerank::{STATUS_CREDENTIALS_MISSING, STATUS_OK};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    Available,
    CredentialsMissing,
}

/// 汎用パスワードとして Keychain に置かれた値を読む。空白だけ・空は未登録扱い。
struct KeychainStore;

impl KeychainStore {
    fn read(account: &str) -> Option<String> {
        let bytes = security_framework::passwords::get_generic_password(rerank::KEYCHAIN_SERVICE, account).ok()?;
        let value = String::from_utf8(bytes).ok()?.trim().to_string();
        (!value.is_empty()).then_some(value)
    }
}

impl CredentialStore for KeychainStore {
    fn load(&self) -> Option<Credentials> {
        Some(Credentials {
            api_token: Self::read(rerank::KEYCHAIN_ACCOUNT_TOKEN)?,
            account_id: Self::read(rerank::KEYCHAIN_ACCOUNT_ID)?,
        })
    }
}

/// Cookie・キャッシュを持たない Agent。タイムアウト(全体)は 3 秒。4xx/5xx はエラーにせず状態コードで返す。
struct UreqTransport {
    agent: ureq::Agent,
}

impl UreqTransport {
    fn new() -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(rerank::REQUEST_TIMEOUT))
            .http_status_as_error(false)
            .build()
            .into();
        Self { agent }
    }
}

impl Transport for UreqTransport {
    fn post(&self, req: &HttpRequest) -> Result<HttpResponse, String> {
        // ureq のエラー文字列は URL (アカウント ID) を含みうるので、中身は捨てる。
        let mut response = self
            .agent
            .post(&req.url)
            .config()
            .timeout_global(Some(req.timeout))
            .build()
            .header("Authorization", &format!("Bearer {}", req.bearer))
            .header("Content-Type", "application/json")
            .send(req.body.as_str())
            .map_err(|_| "transport failed".to_string())?;
        let status = response.status().as_u16();
        if status != 200 {
            return Ok(HttpResponse { status, body: String::new() });
        }
        let body = response.body_mut().read_to_string().map_err(|_| "read failed".to_string())?;
        Ok(HttpResponse { status, body })
    }
}

/// 同時に飛ばす送信の上限。古い送信が 3 秒詰まっても、新しい (Space の) 送信を待たせないための余地。
const MAX_IN_FLIGHT: usize = 2;

/// 送信の管理。状態機械 (`State`)・Keychain・通信を差し替えられるようにして、スレッドの扱いをテストする。
struct Engine<S: CredentialStore, T: Transport> {
    state: Mutex<State<S>>,
    wake: Condvar,
    transport: T,
    /// 送信中のスレッド数。減らすときは必ず `state` のロックを持ち、ワーカーの待機を取りこぼさない。
    in_flight: AtomicUsize,
    /// ワーカーや送信スレッドが落ちたら立てる。以後は何も送らず、ジョブを溜めない (AI は使えない扱い)。
    disabled: AtomicBool,
    /// ワーカーが作った結果。メインキューの drain が取り出す。
    results: Mutex<VecDeque<Delivery>>,
    /// 結果が積まれたことをメインスレッドへ知らせる。
    notify_main: Box<dyn Fn() + Send + Sync>,
    log: Box<dyn Fn(&str) + Send + Sync>,
}

type KeychainEngine = Engine<KeychainStore, UreqTransport>;

type ResultHandler = fn(controller_id: u64, generation: u64, status: i32, text: &str);

static RESULT_HANDLER: OnceLock<ResultHandler> = OnceLock::new();
static SHARED: OnceLock<Arc<KeychainEngine>> = OnceLock::new();

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 結果ハンドラを登録する。最初の 1 回だけ有効。
pub fn set_result_handler(f: ResultHandler) {
    let _ = RESULT_HANDLER.set(f);
}

/// 共有状態。初回にワーカースレッドを起こす。
fn shared() -> &'static Arc<KeychainEngine> {
    SHARED.get_or_init(|| {
        let engine = Arc::new(Engine::new(KeychainStore, UreqTransport::new(), Box::new(notify_main), Box::new(platform::log)));
        engine.start();
        engine
    })
}

fn notify_main() {
    platform::run_on_main(drain_results);
}

/// 送信 1 件ぶんの枠。スレッドが panic しても必ず返す。
struct InFlightSlot<S: CredentialStore + Send + 'static, T: Transport + Send + Sync + 'static>(Arc<Engine<S, T>>);

impl<S: CredentialStore + Send + 'static, T: Transport + Send + Sync + 'static> Drop for InFlightSlot<S, T> {
    fn drop(&mut self) {
        self.0.release_slot();
    }
}

impl<S: CredentialStore + Send + 'static, T: Transport + Send + Sync + 'static> Engine<S, T> {
    fn new(store: S, transport: T, notify_main: Box<dyn Fn() + Send + Sync>, log: Box<dyn Fn(&str) + Send + Sync>) -> Self {
        Self {
            state: Mutex::new(State::new(store)),
            wake: Condvar::new(),
            transport,
            in_flight: AtomicUsize::new(0),
            disabled: AtomicBool::new(false),
            results: Mutex::new(VecDeque::new()),
            notify_main,
            log,
        }
    }

    /// ワーカーを起こす。起こせなければ無効化する。
    fn start(self: &Arc<Self>) {
        let worker = self.clone();
        let spawned = std::thread::Builder::new().name("novakey-llm".into()).spawn(move || {
            if catch_unwind(AssertUnwindSafe(|| worker.worker_loop())).is_err() {
                worker.disable("worker panicked");
            }
        });
        if spawned.is_err() {
            self.disable("worker spawn failed");
        }
    }

    /// AI を使えない扱いにする。ログは 1 度だけ (依頼内容・認証情報は含めない)。
    fn disable(&self, why: &str) {
        if !self.disabled.swap(true, Ordering::SeqCst) {
            (self.log)(&format!("LLM rerank disabled: {why}"));
        }
        lock(&self.state).cancel_all();
        self.wake.notify_all();
    }

    fn is_disabled(&self) -> bool {
        self.disabled.load(Ordering::SeqCst)
    }

    fn release_slot(&self) {
        let _state = lock(&self.state);
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.wake.notify_all();
    }

    /// 時刻が来た送信を、空きがある分だけ取り出し、送信ごとに短命のスレッドを起こす。
    fn worker_loop(self: &Arc<Self>) {
        loop {
            let due = {
                let mut state = lock(&self.state);
                loop {
                    if self.is_disabled() {
                        return;
                    }
                    let free = MAX_IN_FLIGHT.saturating_sub(self.in_flight.load(Ordering::SeqCst));
                    let now = Instant::now();
                    if free > 0 {
                        let due = state.take_due_up_to(now, free);
                        if !due.is_empty() {
                            self.in_flight.fetch_add(due.len(), Ordering::SeqCst);
                            break due;
                        }
                    }
                    state = match state.next_deadline() {
                        // 空きがないときは、送信スレッドの完了通知まで待つ。
                        Some(deadline) if free > 0 => {
                            self.wake
                                .wait_timeout(state, deadline.saturating_duration_since(now))
                                .unwrap_or_else(PoisonError::into_inner)
                                .0
                        }
                        _ => self.wake.wait(state).unwrap_or_else(PoisonError::into_inner),
                    };
                }
            };
            for sent in due {
                (self.log)(&format!("Clef requests sent: {}", sent.request_number));
                let engine = self.clone();
                let spawned = std::thread::Builder::new().name("novakey-llm-send".into()).spawn(move || {
                    let _slot = InFlightSlot(engine.clone());
                    if catch_unwind(AssertUnwindSafe(|| engine.send(sent))).is_err() {
                        engine.disable("sender panicked");
                    }
                });
                if spawned.is_err() {
                    // 起動できなかった枠は、閉じたクロージャごと捨てられ InFlightSlot も作られていない。
                    self.release_slot();
                    self.disable("sender spawn failed");
                }
            }
        }
    }

    fn send(&self, sent: InFlight) {
        let result = {
            // 待っている間に新しい世代が来ていたら、通信しない。
            if !lock(&self.state).is_latest(sent.controller_id, sent.generation) {
                return;
            }
            rerank::fetch_choice(&self.transport, &sent.creds, &sent.key)
        };
        let delivery = lock(&self.state).complete(sent, result);
        if let Some(delivery) = delivery {
            self.push_result(delivery);
        }
    }

    fn push_result(&self, delivery: Delivery) {
        lock(&self.results).push_back(delivery);
        (self.notify_main)();
    }

    fn pop_result(&self) -> Option<Delivery> {
        lock(&self.results).pop_front()
    }

    fn cancel(&self, controller_id: u64) {
        lock(&self.state).cancel(controller_id);
    }

    fn cancel_all(&self) {
        lock(&self.state).cancel_all();
    }

    fn submit(&self, job: Job) {
        if self.is_disabled() {
            return;
        }
        let submitted = lock(&self.state).submit(Instant::now(), job);
        if submitted.log_missing {
            (self.log)("Cloudflare認証情報なし");
        }
        if let Some(delivery) = submitted.delivery {
            self.push_result(delivery);
        }
        self.wake.notify_one();
    }
}

/// メインスレッドで呼ばれる。配信までの間に世代が古くなったものは捨てる。
fn drain_results() {
    let Some(engine) = SHARED.get() else { return };
    while let Some(d) = engine.pop_result() {
        if !lock(&engine.state).is_latest(d.controller_id, d.generation) {
            continue;
        }
        if let Some(handler) = RESULT_HANDLER.get() {
            handler(d.controller_id, d.generation, d.status, &d.text);
        }
    }
}

/// 組成が終わった・外へ送れない・AI をオフにした、のいずれかで、そのコントローラの要求を捨てる。
/// ワーカーをまだ起こしていなければ何もしない。
pub fn cancel(controller_id: u64) {
    if let Some(engine) = SHARED.get() {
        engine.cancel(controller_id);
    }
}

pub fn cancel_all() {
    if let Some(engine) = SHARED.get() {
        engine.cancel_all();
    }
}

/// トグル ON 時の確認。Keychain は、未登録と分かっているときの読み直し以外では触れない。
pub fn availability() -> Availability {
    if lock(&shared().state).availability() {
        Availability::Available
    } else {
        Availability::CredentialsMissing
    }
}

/// urgent は Space 押下時用。デバウンスを待たずに送る(送信済み・キャッシュ命中なら何もしない)。
/// 結果は後でメインスレッドのハンドラに届く。
pub fn rerank_async(
    controller_id: u64,
    generation: u64,
    reading: &str,
    context: &str,
    candidates: &[String],
    urgent: bool,
) {
    if candidates.len() < 2 {
        return;
    }
    shared().submit(Job {
        controller_id,
        generation,
        reading: reading.to_string(),
        context: context.to_string(),
        candidates: candidates.to_vec(),
        urgent,
    });
}

static SMOKE_START: OnceLock<Instant> = OnceLock::new();

/// 実際に 1 回送るスモークテスト (`--test-llm`)。ネットワークを使うので自動テストからは呼ばない。
pub fn smoke_test() {
    println!("availability: {:?}", availability());
    set_result_handler(|_, _, status, text| {
        println!("status={} text={} elapsed={}ms", status, text, SMOKE_START.get().map_or(0, |t| t.elapsed().as_millis()));
        std::process::exit(if status == STATUS_OK { 0 } else { 1 });
    });
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(15));
        eprintln!("timeout: no terminal status within 15s");
        std::process::exit(2);
    });
    let candidates = vec!["今日は医者に行く".to_string(), "今日歯医者に行く".to_string()];
    let _ = SMOKE_START.set(Instant::now());
    rerank_async(0, 0, "きょうはいしゃにいく", "", &candidates, true);
    platform::run_main_run_loop();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    struct Store;

    impl CredentialStore for Store {
        fn load(&self) -> Option<Credentials> {
            Some(Credentials { api_token: "tok".into(), account_id: "acct".into() })
        }
    }

    /// 読みが "slow" の要求は、`release` が来るまで返らない。
    struct Gate {
        slow_started: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
        panic_on: Option<&'static str>,
    }

    impl Transport for Gate {
        fn post(&self, req: &HttpRequest) -> Result<HttpResponse, String> {
            if let Some(word) = self.panic_on {
                if req.body.contains(word) {
                    panic!("fake transport panic");
                }
            }
            if req.body.contains("slow") {
                let _ = self.slow_started.send(());
                let _ = lock(&self.release).recv_timeout(Duration::from_secs(10));
            }
            let body = r#"{"success":true,"result":{"answers":{"rank":{"choice":"1"}}}}"#;
            Ok(HttpResponse { status: 200, body: body.into() })
        }
    }

    struct Harness {
        engine: Arc<Engine<Store, Gate>>,
        slow_started: mpsc::Receiver<()>,
        release: mpsc::Sender<()>,
        delivered: mpsc::Receiver<()>,
        logs: Arc<Mutex<Vec<String>>>,
    }

    fn harness(panic_on: Option<&'static str>) -> Harness {
        let (slow_tx, slow_rx) = mpsc::channel();
        let (rel_tx, rel_rx) = mpsc::channel();
        let (del_tx, del_rx) = mpsc::channel();
        let del_tx = Mutex::new(del_tx);
        let logs = Arc::new(Mutex::new(Vec::new()));
        let log_sink = logs.clone();
        let engine = Arc::new(Engine::new(
            Store,
            Gate { slow_started: slow_tx, release: Mutex::new(rel_rx), panic_on },
            Box::new(move || {
                let _ = lock(&del_tx).send(());
            }),
            Box::new(move |m| lock(&log_sink).push(m.to_string())),
        ));
        engine.start();
        Harness { engine, slow_started: slow_rx, release: rel_tx, delivered: del_rx, logs }
    }

    fn job(id: u64, generation: u64, reading: &str) -> Job {
        Job {
            controller_id: id,
            generation,
            reading: reading.into(),
            context: String::new(),
            candidates: vec!["a".into(), "b".into()],
            urgent: true,
        }
    }

    const WAIT: Duration = Duration::from_secs(5);

    #[test]
    fn an_urgent_request_is_not_blocked_by_a_stale_one_in_flight() {
        let h = harness(None);
        h.engine.submit(job(1, 1, "slow"));
        h.slow_started.recv_timeout(WAIT).expect("古い送信が始まる");
        h.engine.submit(job(1, 2, "fast"));
        h.delivered.recv_timeout(WAIT).expect("古い送信が詰まっていても新しい結果が届く");
        let d = h.engine.pop_result().expect("結果");
        assert_eq!((d.generation, d.status), (2, rerank::STATUS_OK));
        let _ = h.release.send(());
        // 古い世代の結果は、返ってきても配信されない。
        std::thread::sleep(Duration::from_millis(200));
        assert!(h.engine.pop_result().is_none());
    }

    #[test]
    fn cancel_drops_queued_and_in_flight_requests() {
        let h = harness(None);
        h.engine.submit(job(1, 1, "slow"));
        h.slow_started.recv_timeout(WAIT).unwrap();
        h.engine.cancel(1);
        let _ = h.release.send(());
        std::thread::sleep(Duration::from_millis(200));
        assert!(h.engine.pop_result().is_none(), "送信中だった要求の結果は採用しない");
        // デバウンス中 (まだ送っていない) の要求も消える。
        let mut j = job(2, 1, "fast");
        j.urgent = false;
        h.engine.submit(j);
        h.engine.cancel(2);
        assert!(lock(&h.engine.state).next_deadline().is_none());
        std::thread::sleep(Duration::from_millis(400));
        assert!(h.engine.pop_result().is_none());
    }

    #[test]
    fn cancel_all_drops_every_controller() {
        let h = harness(None);
        let mut a = job(1, 1, "fast");
        a.urgent = false;
        let mut b = job(2, 1, "fast");
        b.urgent = false;
        h.engine.submit(a);
        h.engine.submit(b);
        h.engine.cancel_all();
        std::thread::sleep(Duration::from_millis(400));
        assert!(h.engine.pop_result().is_none());
    }

    #[test]
    fn in_flight_requests_are_capped() {
        let h = harness(None);
        for id in 1..=3 {
            h.engine.submit(job(id, 1, "slow"));
        }
        h.slow_started.recv_timeout(WAIT).unwrap();
        h.slow_started.recv_timeout(WAIT).unwrap();
        assert!(h.slow_started.recv_timeout(Duration::from_millis(300)).is_err(), "3 件目は空きを待つ");
        assert_eq!(h.engine.in_flight.load(Ordering::SeqCst), MAX_IN_FLIGHT);
        let _ = h.release.send(());
        h.slow_started.recv_timeout(WAIT).expect("空きができたら 3 件目が始まる");
        let _ = h.release.send(());
        let _ = h.release.send(());
    }

    #[test]
    fn a_panicking_sender_disables_ai_and_logs_once() {
        let h = harness(Some("boom"));
        h.engine.submit(job(1, 1, "boom"));
        let deadline = Instant::now() + WAIT;
        while !h.engine.is_disabled() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(h.engine.is_disabled());
        h.engine.submit(job(1, 2, "fast"));
        h.engine.submit(job(1, 3, "fast"));
        std::thread::sleep(Duration::from_millis(100));
        assert!(h.engine.pop_result().is_none(), "無効化後は何も送らない");
        assert!(lock(&h.engine.state).next_deadline().is_none(), "ジョブを溜めない");
        let logs = lock(&h.logs);
        assert_eq!(logs.iter().filter(|m| m.contains("disabled")).count(), 1, "{logs:?}");
        assert!(!logs.iter().any(|m| m.contains("boom")), "依頼内容はログに出さない");
    }
}
