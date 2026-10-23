//! AI 並べ替えの実行部分。判断は `rerank::State`、ここは Keychain (security-framework)・HTTPS (ureq + rustls)・
//! ワーカースレッド・メインスレッドへの結果配信を受け持つ。unsafe は持たない (メインキューへの投入は `platform`)。
//!
//! 契約: 結果は `MainQueue` を通ってメインスレッドへ渡る (本番の `TaskQueue` は、ハンドラを呼ぶ直前に世代を見直して古いものを捨てる)。認証情報・トークン・リクエスト本文はログに出さない。
#![forbid(unsafe_code)]

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Instant;

use crate::outbound::{self, Decision, Screened};
use crate::platform;
use crate::rerank::{
    self, CredentialStore, Credentials, HttpRequest, HttpResponse, InFlight, Job, State, StoreError, Transport, TransportFailure,
};

pub use crate::rerank::{Availability, Choice, Delivery, Unavailable};

/// 時刻の取得元。デバウンスや待ち時間をテストで進められるようにする。
trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// errSecItemNotFound。項目がない (未登録) ことを表す。
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

/// 汎用パスワードとして Keychain に置かれた値を読む。空白だけ・空・UTF-8 でないものは未登録扱い。
/// 項目がないことと、読めなかったこと (ロック中・拒否など) は区別する。
struct KeychainStore;

impl KeychainStore {
    fn read(account: &str) -> Result<Option<String>, StoreError> {
        let bytes = match security_framework::passwords::get_generic_password(rerank::KEYCHAIN_SERVICE, account) {
            Ok(bytes) => bytes,
            Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => return Ok(None),
            Err(_) => return Err(StoreError),
        };
        let Ok(value) = String::from_utf8(bytes) else { return Ok(None) };
        let value = value.trim().to_string();
        Ok((!value.is_empty()).then_some(value))
    }
}

impl CredentialStore for KeychainStore {
    fn load(&self) -> Result<Option<Credentials>, StoreError> {
        let Some(api_token) = Self::read(rerank::KEYCHAIN_ACCOUNT_TOKEN)? else { return Ok(None) };
        let Some(account_id) = Self::read(rerank::KEYCHAIN_ACCOUNT_ID)? else { return Ok(None) };
        Ok(Some(Credentials { api_token, account_id }))
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
    fn post(&self, req: &HttpRequest) -> Result<HttpResponse, TransportFailure> {
        // ureq のエラー文字列は URL (アカウント ID) を含みうるので、種類だけ取って中身は捨てる。
        let mut response = self
            .agent
            .post(&req.url)
            .config()
            .timeout_global(Some(req.timeout))
            .build()
            .header("Authorization", &format!("Bearer {}", req.bearer))
            .header("Content-Type", "application/json")
            .send(req.body.as_str())
            .map_err(|e| match e {
                ureq::Error::Timeout(_) => TransportFailure::Timeout,
                ureq::Error::HostNotFound | ureq::Error::ConnectionFailed => TransportFailure::Connect,
                ureq::Error::Tls(_) | ureq::Error::Rustls(_) => TransportFailure::Tls,
                _ => TransportFailure::Read,
            })?;
        let status = response.status().as_u16();
        if status != 200 {
            return Ok(HttpResponse { status, body: String::new() });
        }
        let body = response.body_mut().read_to_string().map_err(|_| TransportFailure::Read)?;
        Ok(HttpResponse { status, body })
    }
}

/// 送信の管理。状態機械 (`State`)・Keychain・通信を差し替えられるようにして、スレッドの扱いをテストする。
struct Engine<S: CredentialStore, T: Transport> {
    state: Mutex<State>,
    /// 読み出しは止まりうるので、`state` のロックを持たずにワーカースレッドだけが呼ぶ。
    store: S,
    wake: Condvar,
    transport: T,
    clock: Box<dyn Clock>,
    /// ワーカーが作った結果を、メインスレッドへ渡す。
    main: Arc<dyn MainQueue>,
    log: Box<dyn Fn(&str) + Send + Sync>,
}

type KeychainEngine = Engine<KeychainStore, UreqTransport>;

/// 結果をメインスレッドへ渡す口。`deliver` はワーカーのスレッドから呼ばれる。
pub trait MainQueue: Send + Sync {
    fn deliver(&self, d: Delivery);
}

/// 配信されようとしている結果が、いまも最新の世代のものか。
type IsCurrent = Arc<dyn Fn(&Delivery) -> bool + Send + Sync>;

/// メインスレッドで動く処理を投入する関数 (本番は `platform::run_on_main_boxed`)。
type Post = Box<dyn Fn(Box<dyn FnOnce() + Send>) + Send + Sync>;

/// 結果を「メインスレッドで handler を呼ぶ処理」として `post` で投入する。
/// 投入から実行までの間に取り消されたり新しい世代が来たりした結果は、実行時に捨てる。
struct TaskQueue {
    is_current: IsCurrent,
    post: Post,
    handler: Arc<dyn Fn(Delivery) + Send + Sync>,
}

impl MainQueue for TaskQueue {
    fn deliver(&self, d: Delivery) {
        let (is_current, handler) = (self.is_current.clone(), self.handler.clone());
        (self.post)(Box::new(move || {
            if is_current(&d) {
                handler(d);
            }
        }));
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 送信 1 件ぶんの枠。スレッドが panic しても必ず返す。
struct InFlightSlot<S: CredentialStore + Send + 'static, T: Transport + Send + Sync + 'static>(Arc<Engine<S, T>>);

impl<S: CredentialStore + Send + 'static, T: Transport + Send + Sync + 'static> Drop for InFlightSlot<S, T> {
    fn drop(&mut self) {
        self.0.release_slot();
    }
}

impl<S: CredentialStore + Send + 'static, T: Transport + Send + Sync + 'static> Engine<S, T> {
    /// `make_main` は、結果の古さを判定する関数を受け取って `MainQueue` を作る (判定にはこのエンジン自身の状態が要る)。
    fn new(
        store: S,
        transport: T,
        clock: Box<dyn Clock>,
        log: Box<dyn Fn(&str) + Send + Sync>,
        make_main: impl FnOnce(IsCurrent) -> Arc<dyn MainQueue>,
    ) -> Arc<Self> {
        Arc::new_cyclic(|weak: &std::sync::Weak<Self>| {
            let weak = weak.clone();
            let is_current: IsCurrent = Arc::new(move |d| weak.upgrade().is_some_and(|e| e.is_latest(d)));
            Self {
                state: Mutex::new(State::new()),
                store,
                wake: Condvar::new(),
                transport,
                clock,
                main: make_main(is_current),
                log,
            }
        })
    }

    fn is_latest(&self, d: &Delivery) -> bool {
        lock(&self.state).is_latest(d.controller_id, d.generation)
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
        if lock(&self.state).disable() {
            (self.log)(&format!("LLM rerank disabled: {why}"));
        }
        self.wake.notify_all();
    }

    fn release_slot(&self) {
        lock(&self.state).release_slot();
        self.wake.notify_all();
    }

    /// 認証情報の読み出しと、時刻が来た送信の取り出し (空きがある分だけ。送信ごとに短命のスレッドを起こす)。
    fn worker_loop(self: &Arc<Self>) {
        loop {
            let due = {
                let mut state = lock(&self.state);
                loop {
                    if state.is_disabled() {
                        return;
                    }
                    if state.take_load_request() {
                        drop(state);
                        self.load_credentials();
                        state = lock(&self.state);
                        continue;
                    }
                    let now = self.clock.now();
                    let due = state.take_due(now);
                    if !due.is_empty() {
                        break due;
                    }
                    state = match state.next_deadline() {
                        // 空きがないときは、送信スレッドの完了通知まで待つ。
                        Some(deadline) if state.has_free_slot() => {
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

    /// ロックを外して読み、もう一度ロックを取って結果を渡す。止まっても main の submit は待たない。
    fn load_credentials(&self) {
        let loaded = self.store.load();
        let loaded = lock(&self.state).credentials_loaded(loaded);
        if loaded.log_missing {
            (self.log)("Cloudflare認証情報なし");
        }
        for delivery in loaded.deliveries {
            self.push_result(delivery);
        }
    }

    /// 認証情報の読み出しを先に始めさせる (トグル ON 時)。
    fn prepare(&self) {
        lock(&self.state).request_credentials();
        self.wake.notify_one();
    }

    fn send(&self, sent: InFlight) {
        let result = {
            // 待っている間に新しい世代が来ていたら、通信しない。
            if !lock(&self.state).is_latest(sent.controller_id, sent.generation) {
                return;
            }
            rerank::fetch_choice(&self.transport, &sent.creds, &sent.key)
        };
        let completed = lock(&self.state).complete(self.clock.now(), sent, result);
        if let Some(line) = completed.log {
            (self.log)(&line);
        }
        if let Some(delivery) = completed.delivery {
            self.push_result(delivery);
        }
        self.wake.notify_all();
    }

    fn push_result(&self, delivery: Delivery) {
        self.main.deliver(delivery);
    }

    fn cancel(&self, controller_id: u64) {
        lock(&self.state).cancel(controller_id);
    }

    fn cancel_all(&self) {
        lock(&self.state).cancel_all();
    }

    fn submit(&self, job: Job) {
        let submitted = lock(&self.state).submit(self.clock.now(), job);
        if submitted.log_missing {
            (self.log)("Cloudflare認証情報なし");
        }
        if let Some(delivery) = submitted.delivery {
            self.push_result(delivery);
        }
        self.wake.notify_one();
    }
}

/// AI 並べ替えの送信側。スケジューラ・ワーカースレッド・Keychain・通信を持つ。
pub struct RerankService {
    engine: Arc<KeychainEngine>,
}

impl RerankService {
    /// ワーカースレッドを起こす。`handler` は、メインスレッドで、まだ最新の世代の結果に対してだけ呼ばれる。
    pub fn start(handler: fn(Delivery)) -> Arc<Self> {
        let engine = Engine::new(KeychainStore, UreqTransport::new(), Box::new(SystemClock), Box::new(platform::log), |is_current| {
            Arc::new(TaskQueue { is_current, post: Box::new(platform::run_on_main_boxed), handler: Arc::new(handler) })
        });
        engine.start();
        Arc::new(Self { engine })
    }

    /// 組成が終わった・外へ送れない・AI をオフにした、のいずれかで、そのコントローラの要求を捨てる。
    pub fn cancel(&self, controller_id: u64) {
        self.engine.cancel(controller_id);
    }

    pub fn cancel_all(&self) {
        self.engine.cancel_all();
    }

    /// トグル ON 時に、認証情報の読み出しをバックグラウンドで始めさせる。待たない。
    pub fn prepare(&self) {
        self.engine.prepare();
    }

    /// 今分かっている状態を返すだけで、Keychain には触れない (読み出し中・未読込は使えるとみなす)。
    pub fn availability(&self) -> Availability {
        lock(&self.engine.state).availability(self.engine.clock.now())
    }

    /// urgent は Space 押下時用。デバウンスを待たずに送る(送信済み・キャッシュ命中なら何もしない)。
    /// 結果は後でメインスレッドのハンドラに届く。
    pub fn submit(&self, controller_id: u64, generation: u64, payload: Screened, urgent: bool) {
        if payload.candidates().len() < 2 {
            return;
        }
        self.engine.submit(Job { controller_id, generation, payload, urgent });
    }
}

static SMOKE_START: OnceLock<Instant> = OnceLock::new();

/// 実際に 1 回送るスモークテスト (`--test-llm`)。ネットワークを使うので自動テストからは呼ばない。
pub fn smoke_test() {
    let service = RerankService::start(|d| {
        let elapsed = SMOKE_START.get().map_or(0, |t| t.elapsed().as_millis());
        match d.outcome {
            Ok(choice) => {
                println!("ok index={} elapsed={}ms", choice.index, elapsed);
                std::process::exit(0);
            }
            Err(reason) => {
                println!("unavailable={:?} elapsed={}ms", reason, elapsed);
                std::process::exit(1);
            }
        }
    });
    println!("availability: {:?}", service.availability());
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(15));
        eprintln!("timeout: no terminal status within 15s");
        std::process::exit(2);
    });
    let candidates = vec!["今日は医者に行く".to_string(), "今日歯医者に行く".to_string()];
    // 合成した通常アプリの bundle id で通す (実在のユーザー入力ではないので、アプリ検査は形だけ)。
    let pass = outbound::gate_app(false, Some("com.apple.TextEdit")).expect("通常アプリは通る");
    let Decision::Send(payload) = outbound::screen(pass, "きょうはいしゃにいく", &candidates, "") else {
        eprintln!("smoke test input was rejected by the outbound screen");
        std::process::exit(2);
    };
    let _ = SMOKE_START.set(Instant::now());
    service.submit(0, 0, payload, true);
    platform::run_main_run_loop();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// 呼ばれたスレッドを記録する。`release` があるときは、それが来るまで読み出しが返らない。
    struct Store {
        threads: Mutex<Vec<std::thread::ThreadId>>,
        entered: Mutex<mpsc::Sender<()>>,
        release: Option<Mutex<mpsc::Receiver<()>>>,
    }

    impl CredentialStore for Store {
        fn load(&self) -> Result<Option<Credentials>, StoreError> {
            lock(&self.threads).push(std::thread::current().id());
            let _ = lock(&self.entered).send(());
            if let Some(release) = &self.release {
                let _ = lock(release).recv_timeout(Duration::from_secs(10));
            }
            Ok(Some(Credentials { api_token: "tok".into(), account_id: "acct".into() }))
        }
    }

    /// 読みが "slow" の要求は、`release` が来るまで返らない。
    struct Gate {
        slow_started: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
        panic_on: Option<&'static str>,
        /// `Some` の間は、本文に関わらずこの結果を返す。
        script: Mutex<Option<Result<u16, TransportFailure>>>,
        posts: std::sync::atomic::AtomicUsize,
    }

    impl Transport for Gate {
        fn post(&self, req: &HttpRequest) -> Result<HttpResponse, TransportFailure> {
            self.posts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(scripted) = *lock(&self.script) {
                return scripted.map(|status| HttpResponse { status, body: String::new() });
            }
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

    /// 進めるまで動かない時計。
    struct ManualClock(Mutex<Instant>);

    impl Clock for Arc<ManualClock> {
        fn now(&self) -> Instant {
            *lock(&self.0)
        }
    }

    impl ManualClock {
        fn advance(&self, by: Duration) {
            let mut t = lock(&self.0);
            *t += by;
        }
    }

    /// 配信された結果をためる `MainQueue`。届くたびに `signal` へも知らせる。
    struct Recorder {
        results: Mutex<std::collections::VecDeque<Delivery>>,
        signal: Mutex<mpsc::Sender<()>>,
    }

    impl MainQueue for Arc<Recorder> {
        fn deliver(&self, d: Delivery) {
            lock(&self.results).push_back(d);
            let _ = lock(&self.signal).send(());
        }
    }

    struct Harness {
        engine: Arc<Engine<Store, Gate>>,
        recorder: Arc<Recorder>,
        clock: Arc<ManualClock>,
        slow_started: mpsc::Receiver<()>,
        release: mpsc::Sender<()>,
        delivered: mpsc::Receiver<()>,
        logs: Arc<Mutex<Vec<String>>>,
        store_entered: mpsc::Receiver<()>,
        store_release: mpsc::Sender<()>,
    }

    impl Harness {
        fn pop_result(&self) -> Option<Delivery> {
            lock(&self.recorder.results).pop_front()
        }

        /// 時計を進めてワーカーを起こす。
        fn advance(&self, by: Duration) {
            self.clock.advance(by);
            let _state = lock(&self.engine.state);
            self.engine.wake.notify_all();
        }

        /// 状態が条件を満たすまで待つ。ワーカー自身が変えた状態には通知がないので、短い間隔でも見直す。
        /// 「何も起きないこと」を待つためには使わない。
        fn wait_until(&self, what: &str, ready: impl Fn(&State) -> bool) {
            let deadline = Instant::now() + WAIT;
            let mut state = lock(&self.engine.state);
            while !ready(&state) {
                assert!(Instant::now() < deadline, "待っていた状態にならなかった: {what}");
                state = self
                    .engine
                    .wake
                    .wait_timeout(state, Duration::from_millis(20))
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
        }

        fn wait_idle(&self) {
            self.wait_until("送信中がなくなる", |s| s.in_flight() == 0);
        }
    }

    fn harness(panic_on: Option<&'static str>) -> Harness {
        harness_with(panic_on, false)
    }

    fn harness_with(panic_on: Option<&'static str>, hang_store: bool) -> Harness {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (store_rel_tx, store_rel_rx) = mpsc::channel();
        let (slow_tx, slow_rx) = mpsc::channel();
        let (rel_tx, rel_rx) = mpsc::channel();
        let (del_tx, del_rx) = mpsc::channel();
        let recorder = Arc::new(Recorder { results: Mutex::new(Default::default()), signal: Mutex::new(del_tx) });
        let main = recorder.clone();
        let logs = Arc::new(Mutex::new(Vec::new()));
        let log_sink = logs.clone();
        let clock = Arc::new(ManualClock(Mutex::new(Instant::now())));
        let store = Store {
            threads: Mutex::new(Vec::new()),
            entered: Mutex::new(entered_tx),
            release: hang_store.then(|| Mutex::new(store_rel_rx)),
        };
        let engine = Engine::new(
            store,
            Gate { slow_started: slow_tx, release: Mutex::new(rel_rx), panic_on, script: Mutex::new(None), posts: Default::default() },
            Box::new(clock.clone()),
            Box::new(move |m| lock(&log_sink).push(m.to_string())),
            |_| Arc::new(main),
        );
        engine.start();
        Harness { engine, recorder, clock, slow_started: slow_rx, release: rel_tx, delivered: del_rx, logs, store_entered: entered_rx, store_release: store_rel_tx }
    }

    fn job(id: u64, generation: u64, reading: &str) -> Job {
        let candidates = vec!["a".to_string(), "b".to_string()];
        let pass = outbound::gate_app(false, Some("com.apple.TextEdit")).unwrap();
        let Decision::Send(payload) = outbound::screen(pass, reading, &candidates, "") else { panic!("テスト入力が検査で止まった") };
        Job { controller_id: id, generation, payload, urgent: true }
    }

    const WAIT: Duration = Duration::from_secs(5);

    type Task = Box<dyn FnOnce() + Send>;

    /// 結果を `TaskQueue` 経由で渡す。投入された処理は channel にたまり、テストが「メインスレッドで実行」する。
    struct MainSim {
        engine: Arc<Engine<Store, Gate>>,
        tasks: mpsc::Receiver<Task>,
        handled: Arc<Mutex<Vec<Delivery>>>,
    }

    fn main_sim() -> MainSim {
        let (entered_tx, _entered_rx) = mpsc::channel();
        let (slow_tx, _slow_rx) = mpsc::channel();
        let (_rel_tx, rel_rx) = mpsc::channel();
        let (task_tx, tasks) = mpsc::channel::<Task>();
        let task_tx = Mutex::new(task_tx);
        let handled = Arc::new(Mutex::new(Vec::new()));
        let sink = handled.clone();
        let engine = Engine::new(
            Store { threads: Mutex::new(Vec::new()), entered: Mutex::new(entered_tx), release: None },
            Gate { slow_started: slow_tx, release: Mutex::new(rel_rx), panic_on: None, script: Mutex::new(None), posts: Default::default() },
            Box::new(Arc::new(ManualClock(Mutex::new(Instant::now())))),
            Box::new(|_| {}),
            |is_current| {
                Arc::new(TaskQueue {
                    is_current,
                    post: Box::new(move |task| {
                        let _ = lock(&task_tx).send(task);
                    }),
                    handler: Arc::new(move |d| lock(&sink).push(d)),
                })
            },
        );
        engine.start();
        MainSim { engine, tasks, handled }
    }

    #[test]
    fn a_result_reaches_the_handler_when_main_runs_it() {
        let m = main_sim();
        m.engine.submit(job(1, 1, "fast"));
        let task = m.tasks.recv_timeout(WAIT).expect("結果がメインへ投入される");
        assert!(lock(&m.handled).is_empty(), "メインが実行するまで handler は呼ばれない");
        task();
        let handled = lock(&m.handled);
        assert_eq!(handled.len(), 1);
        assert_eq!((handled[0].controller_id, handled[0].generation), (1, 1));
        assert_eq!(handled[0].outcome, Ok(Choice { index: 1, text: "b".into() }));
    }

    #[test]
    fn a_result_cancelled_while_waiting_for_main_is_dropped() {
        let m = main_sim();
        m.engine.submit(job(1, 1, "fast"));
        let task = m.tasks.recv_timeout(WAIT).expect("結果がメインへ投入される");
        m.engine.cancel(1);
        task();
        assert!(lock(&m.handled).is_empty(), "取り消しのあとに届いた結果は捨てる");
    }

    #[test]
    fn a_result_superseded_while_waiting_for_main_is_dropped() {
        let m = main_sim();
        m.engine.submit(job(1, 1, "fast"));
        let old = m.tasks.recv_timeout(WAIT).expect("結果がメインへ投入される");
        m.engine.submit(job(1, 2, "fast2"));
        let new = m.tasks.recv_timeout(WAIT).expect("新しい世代の結果も投入される");
        old();
        assert!(lock(&m.handled).is_empty(), "古い世代の結果は捨てる");
        new();
        assert_eq!(lock(&m.handled).iter().map(|d| d.generation).collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn cancel_all_drops_results_waiting_for_main() {
        let m = main_sim();
        m.engine.submit(job(1, 1, "fast"));
        let task = m.tasks.recv_timeout(WAIT).expect("結果がメインへ投入される");
        m.engine.cancel_all();
        task();
        assert!(lock(&m.handled).is_empty());
    }

    #[test]
    fn credentials_are_read_off_the_calling_thread_and_only_once() {
        let h = harness(None);
        h.engine.submit(job(1, 1, "fast"));
        h.delivered.recv_timeout(WAIT).expect("結果が届く");
        h.engine.submit(job(1, 2, "fast2"));
        h.delivered.recv_timeout(WAIT).expect("結果が届く");
        let threads = lock(&h.engine.store.threads);
        assert_eq!(threads.len(), 1, "読み出しは 1 度きり");
        assert_ne!(threads[0], std::thread::current().id(), "呼び出し側 (main) のスレッドでは読まない");
    }

    #[test]
    fn preparing_reads_credentials_in_the_background_before_the_first_send() {
        let h = harness(None);
        h.engine.prepare();
        h.store_entered.recv_timeout(WAIT).expect("トグル ON で読み出しが始まる");
        assert_ne!(lock(&h.engine.store.threads)[0], std::thread::current().id());
        h.engine.submit(job(1, 1, "fast"));
        h.delivered.recv_timeout(WAIT).expect("結果が届く");
        assert_eq!(lock(&h.engine.store.threads).len(), 1, "送信時に読み直さない");
    }

    #[test]
    fn submit_and_cancel_do_not_wait_for_a_stalled_credential_read() {
        let h = harness_with(None, true);
        h.engine.submit(job(1, 1, "fast"));
        h.store_entered.recv_timeout(WAIT).expect("読み出しが始まり、止まる");
        let engine = h.engine.clone();
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            engine.submit(job(1, 2, "fast2"));
            engine.cancel(1);
            let _ = lock(&engine.state).availability(Instant::now());
            let _ = done_tx.send(());
        });
        done_rx.recv_timeout(WAIT).expect("読み出しが止まっていても、ロックで待たされない");
        let _ = h.store_release.send(());
        h.engine.submit(job(1, 3, "fast3"));
        h.delivered.recv_timeout(WAIT).expect("読み出しが済めば送れる");
        assert_eq!(h.pop_result().map(|d| d.generation), Some(3), "取り消した世代 2 の結果は届かない");
        assert!(h.pop_result().is_none());
    }

    #[test]
    fn an_urgent_request_is_not_blocked_by_a_stale_one_in_flight() {
        let h = harness(None);
        h.engine.submit(job(1, 1, "slow"));
        h.slow_started.recv_timeout(WAIT).expect("古い送信が始まる");
        h.engine.submit(job(1, 2, "fast"));
        h.delivered.recv_timeout(WAIT).expect("古い送信が詰まっていても新しい結果が届く");
        let d = h.pop_result().expect("結果");
        assert_eq!((d.generation, d.outcome.is_ok()), (2, true));
        let _ = h.release.send(());
        // 古い世代の結果は、返ってきても配信されない。送信スレッドが終わる (枠が返る) のを待ってから確かめる。
        h.wait_idle();
        assert!(h.pop_result().is_none());
    }

    #[test]
    fn cancel_drops_queued_and_in_flight_requests() {
        let h = harness(None);
        h.engine.submit(job(1, 1, "slow"));
        h.slow_started.recv_timeout(WAIT).unwrap();
        h.engine.cancel(1);
        let _ = h.release.send(());
        h.wait_idle();
        assert!(h.pop_result().is_none(), "送信中だった要求の結果は採用しない");
        // デバウンス中 (まだ送っていない) の要求も消える。時計を十分進めても何も送られない。
        let mut j = job(2, 1, "fast");
        j.urgent = false;
        h.engine.submit(j);
        h.engine.cancel(2);
        h.advance(Duration::from_millis(400));
        let mut state = lock(&h.engine.state);
        assert!(state.next_deadline().is_none());
        assert!(state.take_due(h.clock.now()).is_empty());
        drop(state);
        assert!(h.pop_result().is_none());
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
        h.advance(Duration::from_millis(400));
        let mut state = lock(&h.engine.state);
        assert!(state.next_deadline().is_none());
        assert!(state.take_due(h.clock.now()).is_empty());
        drop(state);
        assert!(h.pop_result().is_none());
    }

    #[test]
    fn a_debounced_request_is_sent_when_the_injected_clock_reaches_its_deadline() {
        let h = harness(None);
        let t0 = h.clock.now();
        let mut j = job(1, 1, "fast");
        j.urgent = false;
        h.engine.submit(j);
        // 先に認証情報の読み出しが済むのを待ち、送信待機に入ったことを確かめる。
        h.wait_until("送信待機に入る", |s| s.next_deadline().is_some());
        assert_eq!(lock(&h.engine.state).next_deadline(), Some(t0 + rerank::DEBOUNCE), "期限は注入した時計から決まる");
        h.advance(rerank::DEBOUNCE);
        h.delivered.recv_timeout(WAIT).expect("時計が期限に達すると送られる");
        assert_eq!(h.pop_result().map(|d| d.generation), Some(1));
    }

    #[test]
    fn in_flight_requests_are_capped() {
        let h = harness(None);
        for id in 1..=3 {
            h.engine.submit(job(id, 1, "slow"));
        }
        h.slow_started.recv_timeout(WAIT).unwrap();
        h.slow_started.recv_timeout(WAIT).unwrap();
        {
            // 送信スレッドが動き出した時点で、枠は 2 つとも埋まっている。3 件目は待機に残る。
            let state = lock(&h.engine.state);
            assert_eq!(state.in_flight(), rerank::MAX_IN_FLIGHT);
            assert!(!state.has_free_slot());
            assert!(state.next_deadline().is_some(), "3 件目は空きを待つ");
        }
        let _ = h.release.send(());
        h.slow_started.recv_timeout(WAIT).expect("空きができたら 3 件目が始まる");
        let _ = h.release.send(());
        let _ = h.release.send(());
    }

    #[test]
    fn a_panicking_sender_disables_ai_and_logs_once() {
        let h = harness(Some("boom"));
        h.engine.submit(job(1, 1, "boom"));
        h.wait_until("無効化される", State::is_disabled);
        assert_eq!(lock(&h.engine.state).availability(h.clock.now()), Availability::Disabled);
        h.engine.submit(job(1, 2, "fast"));
        h.engine.submit(job(1, 3, "fast"));
        assert!(h.pop_result().is_none(), "無効化後は何も送らない");
        assert!(lock(&h.engine.state).next_deadline().is_none(), "ジョブを溜めない");
        let logs = lock(&h.logs);
        assert_eq!(logs.iter().filter(|m| m.contains("disabled")).count(), 1, "{logs:?}");
        assert!(!logs.iter().any(|m| m.contains("boom")), "依頼内容はログに出さない");
    }

    impl Harness {
        fn script(&self, result: Option<Result<u16, TransportFailure>>) {
            *lock(&self.engine.transport.script) = result;
        }

        fn posts(&self) -> usize {
            self.engine.transport.posts.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[test]
    fn a_401_stops_sending_logs_once_and_resumes_after_the_toggle_is_cycled() {
        let h = harness(None);
        h.script(Some(Ok(401)));
        h.engine.submit(job(1, 1, "fast"));
        h.wait_until("認証情報が拒否される", |s| s.availability(Instant::now()) == Availability::CredentialsRejected);
        h.wait_idle();
        assert_eq!(h.posts(), 1);
        assert!(h.pop_result().is_none(), "拒否された要求の結果は配信しない");
        h.engine.submit(job(1, 2, "fast2"));
        h.engine.submit(job(1, 3, "fast3"));
        h.engine.cancel(1);
        assert_eq!(h.posts(), 1, "拒否されたあとは送らない");
        assert!(lock(&h.engine.state).next_deadline().is_none());
        assert_eq!(lock(&h.engine.store.threads).len(), 1, "勝手に読み直さない");
        h.script(None);
        h.engine.prepare();
        h.wait_until("認証情報を読み直して再開できる", |s| s.availability(Instant::now()) == Availability::Ready);
        h.engine.submit(job(1, 4, "fast4"));
        h.delivered.recv_timeout(WAIT).expect("再開後は結果が届く");
        assert_eq!(lock(&h.engine.store.threads).len(), 2);
        let logs = lock(&h.logs);
        let stopped: Vec<_> = logs.iter().filter(|m| m.contains("401")).collect();
        assert_eq!(stopped.len(), 1, "{logs:?}");
        assert!(!logs.iter().any(|m| m.contains("tok") || m.contains("acct") || m.contains("fast")), "{logs:?}");
    }

    #[test]
    fn a_429_pauses_for_a_minute_of_the_injected_clock() {
        let h = harness(None);
        h.script(Some(Ok(429)));
        h.engine.submit(job(1, 1, "fast"));
        h.wait_until("一時停止になる", |s| s.availability(h.clock.now()) == Availability::Paused);
        h.wait_idle();
        h.script(None);
        h.engine.submit(job(1, 2, "fast2"));
        assert_eq!(h.posts(), 1, "止めている間は送らない");
        assert!(h.pop_result().is_none());
        h.advance(Duration::from_secs(59));
        assert_eq!(lock(&h.engine.state).availability(h.clock.now()), Availability::Paused);
        h.advance(Duration::from_secs(1));
        assert_eq!(lock(&h.engine.state).availability(h.clock.now()), Availability::Ready);
        h.engine.submit(job(1, 3, "fast3"));
        h.delivered.recv_timeout(WAIT).expect("60 秒たてば再開する");
        assert_eq!(h.posts(), 2);
        let logs = lock(&h.logs);
        assert_eq!(logs.iter().filter(|m| m.contains("429")).count(), 1, "{logs:?}");
    }

    #[test]
    fn a_timeout_fails_only_that_request() {
        let h = harness(None);
        h.script(Some(Err(TransportFailure::Timeout)));
        h.engine.submit(job(1, 1, "fast"));
        h.delivered.recv_timeout(WAIT).expect("失敗が届く");
        assert_eq!(h.pop_result().map(|d| d.outcome), Some(Err(Unavailable::Failed)));
        h.script(None);
        h.engine.submit(job(1, 2, "fast2"));
        h.delivered.recv_timeout(WAIT).expect("次の要求は普通に送られる");
        assert_eq!(h.pop_result().map(|d| d.outcome.is_ok()), Some(true));
        assert_eq!(h.posts(), 2, "再送しない");
    }

    #[test]
    fn three_bad_responses_in_a_row_disable_and_log_once() {
        let h = harness(None);
        h.script(Some(Ok(404)));
        for g in 1..=3 {
            h.engine.submit(job(1, g, &format!("fast{g}")));
            h.delivered.recv_timeout(WAIT).expect("失敗が届く");
            h.wait_idle();
            let _ = h.pop_result();
        }
        assert_eq!(lock(&h.engine.state).availability(Instant::now()), Availability::Disabled);
        h.engine.submit(job(1, 4, "fast4"));
        assert_eq!(h.posts(), 3);
        let logs = lock(&h.logs);
        assert_eq!(logs.iter().filter(|m| m.contains("disabled")).count(), 1, "{logs:?}");
    }
}
