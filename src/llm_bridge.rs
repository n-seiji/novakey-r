//! AI 並べ替えの実行部分。判断は `rerank::State`、ここは Keychain (security-framework)・HTTPS (ureq + rustls)・
//! ワーカースレッド・メインスレッドへの結果配信を受け持つ。unsafe は持たない (メインキューへの投入は `platform`)。
//!
//! 契約: 結果ハンドラはメインスレッドで呼ばれる。認証情報・トークン・リクエスト本文はログに出さない。
#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Instant;

use crate::platform;
use crate::rerank::{
    self, CredentialStore, Credentials, Delivery, HttpRequest, HttpResponse, Job, State, Transport,
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

struct Shared {
    state: Mutex<State<KeychainStore>>,
    wake: Condvar,
    /// ワーカーが作った結果。メインキューの drain が取り出す。
    results: Mutex<VecDeque<Delivery>>,
}

type ResultHandler = fn(controller_id: u64, generation: u64, status: i32, text: &str);

static RESULT_HANDLER: OnceLock<ResultHandler> = OnceLock::new();
static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 結果ハンドラを登録する。最初の 1 回だけ有効。
pub fn set_result_handler(f: ResultHandler) {
    let _ = RESULT_HANDLER.set(f);
}

/// 共有状態。初回にワーカースレッドを起こす。
fn shared() -> &'static Arc<Shared> {
    SHARED.get_or_init(|| {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::new(KeychainStore)),
            wake: Condvar::new(),
            results: Mutex::new(VecDeque::new()),
        });
        let worker = shared.clone();
        std::thread::Builder::new()
            .name("novakey-llm".into())
            .spawn(move || worker_loop(&worker))
            .expect("LLM ワーカーを起動できません");
        shared
    })
}

/// 時刻が来た送信を取り出し、ロックの外で通信して、結果を返す。
fn worker_loop(shared: &Shared) {
    let transport = UreqTransport::new();
    loop {
        let due = {
            let mut state = lock(&shared.state);
            loop {
                let now = Instant::now();
                let due = state.take_due(now);
                if !due.is_empty() {
                    break due;
                }
                state = match state.next_deadline() {
                    None => shared.wake.wait(state).unwrap_or_else(PoisonError::into_inner),
                    Some(deadline) => {
                        shared
                            .wake
                            .wait_timeout(state, deadline.saturating_duration_since(now))
                            .unwrap_or_else(PoisonError::into_inner)
                            .0
                    }
                };
            }
        };
        for sent in due {
            platform::log(&format!("Clef requests sent: {}", sent.request_number));
            let result = rerank::fetch_choice(&transport, &sent.creds, &sent.key);
            let delivery = lock(&shared.state).complete(sent, result);
            if let Some(delivery) = delivery {
                push_result(shared, delivery);
            }
        }
    }
}

fn push_result(shared: &Shared, delivery: Delivery) {
    lock(&shared.results).push_back(delivery);
    platform::run_on_main(drain_results);
}

/// メインスレッドで呼ばれる。配信までの間に世代が古くなったものは捨てる。
fn drain_results() {
    let Some(shared) = SHARED.get() else { return };
    loop {
        let Some(d) = lock(&shared.results).pop_front() else { return };
        if !lock(&shared.state).is_latest(d.controller_id, d.generation) {
            continue;
        }
        if let Some(handler) = RESULT_HANDLER.get() {
            handler(d.controller_id, d.generation, d.status, &d.text);
        }
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
    let shared = shared();
    let submitted = lock(&shared.state).submit(
        Instant::now(),
        Job {
            controller_id,
            generation,
            reading: reading.to_string(),
            context: context.to_string(),
            candidates: candidates.to_vec(),
            urgent,
        },
    );
    if submitted.log_missing {
        platform::log("Cloudflare認証情報なし");
    }
    if let Some(delivery) = submitted.delivery {
        push_result(shared, delivery);
    }
    shared.wake.notify_one();
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
