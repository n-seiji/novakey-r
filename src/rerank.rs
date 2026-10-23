//! AI 並べ替え (Cloudflare Workers AI の clef-flash) の判断部分。入出力を持たない純粋な状態機械。
//!
//! 通信と Keychain は trait (`Transport` / `CredentialStore`)、時刻は引数 (`now`) で渡すので、
//! デバウンス・最新世代だけを採用する規則・同一リクエストの再利用は、ネットワークなしでテストできる。
//! 呼び出し側 (`llm_bridge`) は `State` を Mutex で包み、`take_due` で取り出した送信と、`take_load_request` で頼まれた
//! 認証情報の読み出しを、ロックの外で実行して `complete` / `credentials_loaded` に返す (ロックの中で I/O をしない)。
#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};

use crate::outbound::Screened;

pub const STATUS_OK: i32 = 0;
pub const STATUS_FAILED: i32 = 1;
pub const STATUS_CREDENTIALS_MISSING: i32 = 4;

pub const KEYCHAIN_SERVICE: &str = "me.sijis.inputmethod.NovakeyR.cloudflare";
pub const KEYCHAIN_ACCOUNT_TOKEN: &str = "api-token";
pub const KEYCHAIN_ACCOUNT_ID: &str = "account-id";

const RANK_INSTRUCTIONS: &str = "読みに対して最も自然なかな漢字変換候補を選べ";
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
pub const DEBOUNCE: Duration = Duration::from_millis(250);
/// 同時に飛ばす送信の上限。古い送信が 3 秒詰まっても、新しい (Space の) 送信を待たせないための余地。
pub const MAX_IN_FLIGHT: usize = 2;
/// 429 / 5xx のあと、送信を止める時間。
pub const PAUSE: Duration = Duration::from_secs(60);
/// 「その他の失敗」がこの回数続いたら無効化する。
pub const MAX_PERMANENT_FAILURES: u32 = 3;

/// AI 並べ替えを今使えるか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    Ready,
    CredentialsMissing,
    CredentialsRejected,
    Paused,
    /// 送信スレッドなどが落ちて、以後は何も送らない。
    Disabled,
}

/// Cloudflare の認証情報。ログやエラーに出さないよう Debug は伏せる。
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub api_token: String,
    pub account_id: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credentials(..)")
    }
}

/// 認証情報を読めなかった (ロック中・アクセス拒否など)。未登録は含まない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreError;

/// Keychain 等から認証情報を読む。未登録は `Ok(None)`、読めなかったときは `Err`。
/// 読み出しは長く止まりうるので、`State` のロックを持ったまま呼ばない。
pub trait CredentialStore: Send + Sync {
    fn load(&self) -> Result<Option<Credentials>, StoreError>;
}

pub struct HttpRequest {
    pub url: String,
    pub bearer: String,
    pub body: String,
    pub timeout: Duration,
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HttpRequest(..)")
    }
}

pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

/// 通信に失敗した理由の種類だけ。ライブラリのエラー文字列は URL (アカウント ID を含む) を含みうるので持たない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportFailure {
    Timeout,
    Connect,
    Tls,
    Read,
}

/// POST を 1 回送る。
pub trait Transport {
    fn post(&self, req: &HttpRequest) -> Result<HttpResponse, TransportFailure>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    Transport(TransportFailure),
    Status(u16),
    InvalidResponse,
}

/// 失敗への対応の区分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// タイムアウト・接続失敗。その要求だけ失敗にし、再送しない。
    Transient,
    /// 429 / 5xx。しばらく送らない。
    Backoff,
    /// 401 / 403。認証情報が拒否された。
    AuthRejected,
    /// その他の 4xx・不正な応答。続くようなら無効化する。
    Permanent,
}

impl FetchError {
    pub fn classify(&self) -> FailureClass {
        match self {
            FetchError::Transport(_) => FailureClass::Transient,
            FetchError::Status(401 | 403) => FailureClass::AuthRejected,
            FetchError::Status(429 | 500..=599) => FailureClass::Backoff,
            FetchError::Status(_) | FetchError::InvalidResponse => FailureClass::Permanent,
        }
    }

    /// ログ用の理由名。本文・URL は含まない。
    fn reason(&self) -> String {
        match self {
            FetchError::Transport(t) => format!("transport {t:?}"),
            FetchError::Status(code) => format!("status {code}"),
            FetchError::InvalidResponse => "invalid response".to_string(),
        }
    }
}

pub fn build_request(creds: &Credentials, key: &Screened) -> HttpRequest {
    let criteria: serde_json::Map<String, serde_json::Value> = key
        .candidates()
        .iter()
        .enumerate()
        .map(|(i, c)| (i.to_string(), serde_json::Value::String(c.clone())))
        .collect();
    let mut state = serde_json::Map::new();
    state.insert("reading".into(), key.reading().into());
    if !key.context().is_empty() {
        state.insert("context".into(), key.context().into());
    }
    let body = serde_json::json!({
        "model": "clef-flash",
        "state": state,
        "questions": {
            "rank": {
                "type": "choice",
                "instructions": RANK_INSTRUCTIONS,
                "criteria": criteria,
            }
        },
    });
    HttpRequest {
        url: format!(
            "https://api.cloudflare.com/client/v4/accounts/{}/ai/run/@cf/cloudflare/clef-flash",
            creds.account_id
        ),
        bearer: creds.api_token.clone(),
        body: body.to_string(),
        timeout: REQUEST_TIMEOUT,
    }
}

/// `result.answers.rank.choice` (候補の添字を表す 10 進文字列) を取り出す。範囲外・表記ゆれ ("01", "+1") は不正。
pub fn parse_choice(body: &str, candidate_count: usize) -> Result<usize, FetchError> {
    let value: serde_json::Value = serde_json::from_str(body).map_err(|_| FetchError::InvalidResponse)?;
    if value.get("success").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err(FetchError::InvalidResponse);
    }
    let choice = value
        .pointer("/result/answers/rank/choice")
        .and_then(serde_json::Value::as_str)
        .ok_or(FetchError::InvalidResponse)?;
    let index: usize = choice.parse().map_err(|_| FetchError::InvalidResponse)?;
    if index.to_string() != choice || index >= candidate_count {
        return Err(FetchError::InvalidResponse);
    }
    Ok(index)
}

pub fn fetch_choice(transport: &dyn Transport, creds: &Credentials, key: &Screened) -> Result<usize, FetchError> {
    let response = transport.post(&build_request(creds, key)).map_err(FetchError::Transport)?;
    if response.status != 200 {
        return Err(FetchError::Status(response.status));
    }
    parse_choice(&response.body, key.candidates().len())
}

pub struct Job {
    pub controller_id: u64,
    pub generation: u64,
    pub payload: Screened,
    /// Space 押下時。デバウンスを待たずに送る。
    pub urgent: bool,
}

struct Pending {
    due: Instant,
    generation: u64,
    key: Screened,
    creds: Credentials,
}

/// `take_due` が取り出した、これから送る 1 件。
pub struct InFlight {
    pub controller_id: u64,
    pub generation: u64,
    /// 送信の通し番号 ("Clef requests sent: N")。
    pub request_number: u64,
    pub key: Screened,
    pub creds: Credentials,
}

/// メインスレッドへ返す結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    pub controller_id: u64,
    pub generation: u64,
    pub status: i32,
    pub text: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Submitted {
    pub delivery: Option<Delivery>,
    /// 認証情報なしを初めて検知した (1 度だけログを出す)。
    pub log_missing: bool,
}

/// `complete` の結果。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Completed {
    pub delivery: Option<Delivery>,
    /// 状態が変わったときだけ入る 1 行 (理由名と数だけ。依頼内容・認証情報は含まない)。
    pub log: Option<String>,
}

/// `credentials_loaded` の結果。読み出し待ちだった要求の分をまとめて返す。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Loaded {
    pub deliveries: Vec<Delivery>,
    pub log_missing: bool,
}

enum CredState {
    /// 未読込。Keychain は最初の送信かトグル ON まで触らない。
    Unknown,
    /// 読み出しが要求された。ワーカーが `take_load_request` で引き取る。
    Wanted,
    /// ワーカーが読み出し中 (ロックの外)。
    Loading,
    Loaded(Credentials),
    /// 読んだが未登録。
    Missing,
    /// 読めなかった (ロック中・拒否など)。使えない扱いは `Missing` と同じ。
    Unreadable,
}

/// 送信を続けてよいか。認証情報の状態 (`CredState`) とは別に、通信の結果から決まる。
enum Health {
    Ready,
    /// 429 / 5xx のあと。この時刻まで送らない。
    PausedUntil(Instant),
    /// 401 / 403。認証情報の読み直し (トグルの入れ直し) まで送らない。
    Rejected,
    /// 落ちた・失敗が続いた。以後は何も受け付けない。
    Disabled,
}

/// 認証情報の読み出し待ちの要求 (コントローラごとに最新の 1 件)。
struct Awaiting {
    since: Instant,
    job: Job,
}

pub struct State {
    creds: CredState,
    awaiting: HashMap<u64, Awaiting>,
    missing_logged: bool,
    latest: HashMap<u64, u64>,
    /// 送信を始めた (または結果を配信した) 世代。urgent の重複送信を防ぐ。
    sent: HashMap<u64, u64>,
    /// 直近 1 件の成功結果。同一リクエストは通信せずこれを返す。
    last_result: Option<(Screened, usize)>,
    request_count: u64,
    pending: HashMap<u64, Pending>,
    /// 送信中の件数 (`take_due` で増やし、`release_slot` で減らす)。上限は `MAX_IN_FLIGHT`。
    in_flight: usize,
    health: Health,
    /// 「その他の失敗」の連続回数。成功で戻る。
    permanent_failures: u32,
}

impl State {
    pub fn new() -> Self {
        Self {
            creds: CredState::Unknown,
            awaiting: HashMap::new(),
            missing_logged: false,
            latest: HashMap::new(),
            sent: HashMap::new(),
            last_result: None,
            request_count: 0,
            pending: HashMap::new(),
            in_flight: 0,
            health: Health::Ready,
            permanent_failures: 0,
        }
    }

    /// 今分かっている状態。未読込・読み出し中は使えるとみなす (送信時に分かる)。読まない。
    pub fn availability(&self, now: Instant) -> Availability {
        match self.health {
            Health::Disabled => return Availability::Disabled,
            Health::Rejected => return Availability::CredentialsRejected,
            Health::PausedUntil(until) if now < until => return Availability::Paused,
            Health::PausedUntil(_) | Health::Ready => {}
        }
        if matches!(self.creds, CredState::Missing | CredState::Unreadable) {
            Availability::CredentialsMissing
        } else {
            Availability::Ready
        }
    }

    /// 使えない扱いにして、待機中の要求を捨てる。最初の 1 回だけ `true`。
    pub fn disable(&mut self) -> bool {
        let first = !self.is_disabled();
        self.health = Health::Disabled;
        self.cancel_all();
        first
    }

    pub fn is_disabled(&self) -> bool {
        matches!(self.health, Health::Disabled)
    }

    #[cfg(test)]
    pub fn in_flight(&self) -> usize {
        self.in_flight
    }

    pub fn has_free_slot(&self) -> bool {
        self.in_flight < MAX_IN_FLIGHT
    }

    /// 送信を終えた (または起こせなかった) 枠を返す。
    pub fn release_slot(&mut self) {
        self.in_flight = self.in_flight.saturating_sub(1);
    }

    /// 認証情報の読み出しを頼む (トグル ON 時)。読み込み済みなら何もしない。
    /// 未登録・読めなかったときは読み直す。実際に読むのはワーカー。
    pub fn request_credentials(&mut self) {
        if self.is_disabled() {
            return;
        }
        if matches!(self.health, Health::Rejected) {
            self.health = Health::Ready;
            self.creds = CredState::Wanted;
        }
        if matches!(self.creds, CredState::Unknown | CredState::Missing | CredState::Unreadable) {
            self.creds = CredState::Wanted;
        }
    }

    /// 読み出しを頼まれていれば引き取る (`true` ならロックを外して `store.load()` し、`credentials_loaded` を呼ぶ)。
    pub fn take_load_request(&mut self) -> bool {
        if matches!(self.creds, CredState::Wanted) {
            self.creds = CredState::Loading;
            return true;
        }
        false
    }

    /// 読み出しの結果を受ける。待っていた要求は、まだ最新のものだけ続きを進める。
    pub fn credentials_loaded(&mut self, result: Result<Option<Credentials>, StoreError>) -> Loaded {
        let loaded = match result {
            Ok(Some(c)) => {
                self.creds = CredState::Loaded(c.clone());
                Some(c)
            }
            Ok(None) => {
                self.creds = CredState::Missing;
                None
            }
            Err(StoreError) => {
                self.creds = CredState::Unreadable;
                None
            }
        };
        let mut out = Loaded::default();
        if loaded.is_none() {
            out.log_missing = self.note_missing();
        }
        let mut waiting: Vec<(u64, Awaiting)> = self.awaiting.drain().collect();
        waiting.sort_unstable_by_key(|(id, _)| *id);
        for (id, a) in waiting {
            if !self.is_latest(id, a.job.generation) {
                continue;
            }
            let submitted = match &loaded {
                Some(c) => self.admit(a.since, a.job, c.clone()),
                None => self.deliver_missing(a.job),
            };
            out.log_missing |= submitted.log_missing;
            out.deliveries.extend(submitted.delivery);
        }
        out
    }

    /// 「認証情報なし」のログを出すべきか (1 度だけ true)。
    fn note_missing(&mut self) -> bool {
        let first = !self.missing_logged;
        self.missing_logged = true;
        first
    }

    fn register_latest(&mut self, id: u64, generation: u64) {
        self.latest.insert(id, generation);
        self.pending.remove(&id);
    }

    pub fn is_latest(&self, id: u64, generation: u64) -> bool {
        self.latest.get(&id) == Some(&generation)
    }

    pub fn submit(&mut self, now: Instant, job: Job) -> Submitted {
        if job.payload.candidates().len() < 2 {
            return Submitted::default();
        }
        match self.health {
            Health::Disabled | Health::Rejected => return Submitted::default(),
            Health::PausedUntil(until) if now < until => return Submitted::default(),
            Health::PausedUntil(_) => self.health = Health::Ready,
            Health::Ready => {}
        }
        match &self.creds {
            CredState::Loaded(c) => {
                let c = c.clone();
                self.admit(now, job, c)
            }
            CredState::Missing | CredState::Unreadable => self.deliver_missing(job),
            CredState::Unknown | CredState::Wanted | CredState::Loading => {
                if matches!(self.creds, CredState::Unknown) {
                    self.creds = CredState::Wanted;
                }
                self.register_latest(job.controller_id, job.generation);
                self.awaiting.insert(job.controller_id, Awaiting { since: now, job });
                Submitted::default()
            }
        }
    }

    fn deliver_missing(&mut self, job: Job) -> Submitted {
        let log_missing = self.note_missing();
        self.register_latest(job.controller_id, job.generation);
        Submitted {
            delivery: Some(Delivery {
                controller_id: job.controller_id,
                generation: job.generation,
                status: STATUS_CREDENTIALS_MISSING,
                text: "Cloudflare認証情報なし".into(),
            }),
            log_missing,
        }
    }

    fn admit(&mut self, now: Instant, job: Job, creds: Credentials) -> Submitted {
        let Job { controller_id: id, generation, payload: key, urgent } = job;
        if let Some((cached_key, index)) = &self.last_result {
            // key には context_dropped も含まれる。送る本文が同じでも理由が違えば外れるだけで、漏れは増えない (キャッシュ命中が減る側にしか倒れない)。
            if *cached_key == key {
                let text = index.to_string();
                self.register_latest(id, generation);
                self.sent.insert(id, generation);
                return Submitted {
                    delivery: Some(Delivery { controller_id: id, generation, status: STATUS_OK, text }),
                    log_missing: false,
                };
            }
        }
        if urgent && self.sent.get(&id) == Some(&generation) && self.is_latest(id, generation) {
            return Submitted::default();
        }
        self.register_latest(id, generation);
        let delay = if urgent { Duration::ZERO } else { DEBOUNCE };
        self.pending.insert(id, Pending { due: now + delay, generation, key, creds });
        Submitted::default()
    }

    /// 次に送る時刻 (待機中がなければ `None`)。
    pub fn next_deadline(&self) -> Option<Instant> {
        self.pending.values().map(|p| p.due).min()
    }

    /// 時刻が来た送信を、空き枠の分だけ取り出す。取り出した時点で「送信済み」として数え、枠を埋める。
    /// 残りは待機に残り、枠が返ったあとの呼び出しで取り出す。
    pub fn take_due(&mut self, now: Instant) -> Vec<InFlight> {
        let free = MAX_IN_FLIGHT.saturating_sub(self.in_flight);
        if free == 0 {
            return Vec::new();
        }
        let out = self.take_due_up_to(now, free);
        self.in_flight += out.len();
        out
    }

    /// 枠を数えずに取り出す。
    fn take_due_up_to(&mut self, now: Instant, max: usize) -> Vec<InFlight> {
        let mut ids: Vec<u64> = self.pending.iter().filter(|(_, p)| p.due <= now).map(|(id, _)| *id).collect();
        ids.sort_unstable();
        ids.truncate(max);
        let mut out = Vec::new();
        for id in ids {
            let Some(p) = self.pending.remove(&id) else { continue };
            if !self.is_latest(id, p.generation) {
                continue;
            }
            self.sent.insert(id, p.generation);
            self.request_count += 1;
            out.push(InFlight {
                controller_id: id,
                generation: p.generation,
                request_number: self.request_count,
                key: p.key,
                creds: p.creds,
            });
        }
        out
    }

    /// 1 つのコントローラの待機中の要求を捨て、送信中のものも結果を採用しないようにする
    /// (世代の記録ごと消すので、組成が終わったコントローラの分も残らない)。
    pub fn cancel(&mut self, id: u64) {
        self.pending.remove(&id);
        self.awaiting.remove(&id);
        self.latest.remove(&id);
        self.sent.remove(&id);
    }

    /// 全コントローラぶん `cancel` する (AI をオフにしたとき)。
    pub fn cancel_all(&mut self) {
        self.pending.clear();
        self.awaiting.clear();
        self.latest.clear();
        self.sent.clear();
    }

    /// 通信の結果を受ける。失敗は分類に従って、送信の停止・一時停止・無効化のどれかへ進める。
    /// 古い世代の結果も、状態の判断には使う (配信だけ捨てる)。
    pub fn complete(&mut self, now: Instant, sent: InFlight, result: Result<usize, FetchError>) -> Completed {
        let latest = self.is_latest(sent.controller_id, sent.generation);
        let mut log = None;
        let (status, text) = match result {
            Ok(index) => {
                self.permanent_failures = 0;
                if latest {
                    self.last_result = Some((sent.key, index));
                }
                (STATUS_OK, index.to_string())
            }
            Err(err) => {
                let class = err.classify();
                log = self.note_failure(now, &err, class);
                if matches!(class, FailureClass::AuthRejected | FailureClass::Backoff) {
                    // 止めた要求の結果は配信しない。メインスレッドは availability から状態を知る。
                    return Completed { delivery: None, log };
                }
                (STATUS_FAILED, "Clef rerank failed".to_string())
            }
        };
        let delivery = latest
            .then(|| Delivery { controller_id: sent.controller_id, generation: sent.generation, status, text });
        Completed { delivery, log }
    }

    /// 失敗を状態に反映する。状態が変わったときだけログの 1 行を返す (理由名と数だけ)。
    fn note_failure(&mut self, now: Instant, err: &FetchError, class: FailureClass) -> Option<String> {
        match class {
            FailureClass::Transient => None,
            FailureClass::AuthRejected => {
                if matches!(self.health, Health::Rejected | Health::Disabled) {
                    return None;
                }
                self.health = Health::Rejected;
                self.creds = CredState::Unknown;
                self.pending.clear();
                self.awaiting.clear();
                Some(format!("LLM rerank stopped: credentials rejected ({})", err.reason()))
            }
            FailureClass::Backoff => {
                if matches!(self.health, Health::Rejected | Health::Disabled) {
                    return None;
                }
                if matches!(self.health, Health::PausedUntil(until) if now < until) {
                    return None;
                }
                self.health = Health::PausedUntil(now + PAUSE);
                self.pending.clear();
                Some(format!("LLM rerank paused for {}s ({})", PAUSE.as_secs(), err.reason()))
            }
            FailureClass::Permanent => {
                self.permanent_failures += 1;
                if self.permanent_failures < MAX_PERMANENT_FAILURES || self.is_disabled() {
                    return None;
                }
                self.disable()
                    .then(|| format!("LLM rerank disabled: {} consecutive failures ({})", self.permanent_failures, err.reason()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outbound::{self, Decision};
    use std::cell::RefCell;
    use std::ops::{Deref, DerefMut};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    impl State {
        /// 空き枠を無視して、時刻が来たものを全部取り出す。
        fn take_all_due(&mut self, now: Instant) -> Vec<InFlight> {
            self.take_due_up_to(now, usize::MAX)
        }
    }

    #[derive(Clone, Default)]
    struct Loads(Arc<AtomicU32>);

    impl Loads {
        fn get(&self) -> u32 {
            self.0.load(Ordering::SeqCst)
        }
    }

    struct Store {
        result: Mutex<Result<Option<Credentials>, StoreError>>,
        loads: Loads,
    }

    impl CredentialStore for Store {
        fn load(&self) -> Result<Option<Credentials>, StoreError> {
            self.loads.0.fetch_add(1, Ordering::SeqCst);
            self.result.lock().unwrap().clone()
        }
    }

    /// `State` と fake の store。ワーカーがやる「読み出しを頼まれたら読んで結果を渡す」を `submit` の直後に同期で行う。
    struct Rig {
        state: State,
        store: Store,
    }

    impl Deref for Rig {
        type Target = State;
        fn deref(&self) -> &State {
            &self.state
        }
    }

    impl DerefMut for Rig {
        fn deref_mut(&mut self) -> &mut State {
            &mut self.state
        }
    }

    impl Rig {
        /// 頼まれていれば store を読んで結果を渡す (ワーカーの `load_credentials` 相当)。
        fn pump(&mut self) -> Loaded {
            if self.state.take_load_request() {
                self.state.credentials_loaded(self.store.load())
            } else {
                Loaded::default()
            }
        }

        /// 提出し、読み出しがあればそれも済ませて、最初の結果を `delivery` に載せる。
        fn submit(&mut self, now: Instant, job: Job) -> Submitted {
            let mut out = self.state.submit(now, job);
            let loaded = self.pump();
            out.log_missing |= loaded.log_missing;
            if out.delivery.is_none() {
                out.delivery = loaded.deliveries.into_iter().next();
            }
            out
        }
    }

    fn creds() -> Credentials {
        Credentials { api_token: "tok".into(), account_id: "acct".into() }
    }

    fn rig(result: Result<Option<Credentials>, StoreError>) -> (Rig, Loads) {
        let loads = Loads::default();
        let store = Store { result: Mutex::new(result), loads: loads.clone() };
        (Rig { state: State::new(), store }, loads)
    }

    fn state(with_creds: bool) -> (Rig, Loads) {
        rig(Ok(with_creds.then(creds)))
    }

    fn screened(context: &str, reading: &str, candidates: &[&str]) -> Screened {
        let candidates: Vec<String> = candidates.iter().map(|c| c.to_string()).collect();
        let pass = outbound::gate_app(false, Some("com.apple.TextEdit")).unwrap();
        match outbound::screen(pass, reading, &candidates, context) {
            Decision::Send(s) => s,
            Decision::Skip(reason) => panic!("テスト入力が検査で止まった: {}", reason.name()),
        }
    }

    fn job(id: u64, generation: u64, reading: &str, urgent: bool) -> Job {
        Job { controller_id: id, generation, payload: screened("", reading, &["今日は", "京は"]), urgent }
    }

    fn ms(base: Instant, n: u64) -> Instant {
        base + Duration::from_millis(n)
    }

    #[test]
    fn request_has_endpoint_bearer_and_body() {
        let key = screened("前文", "きょうは", &["a", "b"]);
        let req = build_request(&creds(), &key);
        assert_eq!(
            req.url,
            "https://api.cloudflare.com/client/v4/accounts/acct/ai/run/@cf/cloudflare/clef-flash"
        );
        assert_eq!(req.bearer, "tok");
        assert_eq!(req.timeout, Duration::from_secs(3));
        let body: serde_json::Value = serde_json::from_str(&req.body).unwrap();
        assert_eq!(body["model"], "clef-flash");
        assert_eq!(body["state"]["reading"], "きょうは");
        assert_eq!(body["state"]["context"], "前文");
        assert_eq!(body["questions"]["rank"]["type"], "choice");
        assert_eq!(body["questions"]["rank"]["instructions"], RANK_INSTRUCTIONS);
        assert_eq!(body["questions"]["rank"]["criteria"]["0"], "a");
        assert_eq!(body["questions"]["rank"]["criteria"]["1"], "b");
    }

    #[test]
    fn request_omits_empty_context() {
        let key = screened("", "あ", &["a", "b"]);
        let body: serde_json::Value = serde_json::from_str(&build_request(&creds(), &key).body).unwrap();
        assert!(body["state"].get("context").is_none());
    }

    #[test]
    fn debug_output_hides_secrets() {
        let key = screened("", "あ", &[]);
        let text = format!("{:?} {:?}", creds(), build_request(&creds(), &key));
        assert!(!text.contains("tok") && !text.contains("acct"), "{text}");
    }

    #[test]
    fn choice_is_parsed_only_when_well_formed_and_in_range() {
        let ok = |c: &str| format!(r#"{{"success":true,"result":{{"answers":{{"rank":{{"choice":"{c}"}}}}}}}}"#);
        assert_eq!(parse_choice(&ok("1"), 2), Ok(1));
        for bad in ["2", "-1", "01", "+1", "x", ""] {
            assert_eq!(parse_choice(&ok(bad), 2), Err(FetchError::InvalidResponse), "{bad}");
        }
        assert!(parse_choice(r#"{"success":false,"result":{"answers":{"rank":{"choice":"0"}}}}"#, 2).is_err());
        assert!(parse_choice(r#"{"success":true}"#, 2).is_err());
        assert!(parse_choice(r#"{"success":true,"result":{"answers":{"rank":{"choice":1}}}}"#, 2).is_err());
        assert!(parse_choice("not json", 2).is_err());
    }

    struct Fake(RefCell<Vec<HttpRequest>>, Result<(u16, &'static str), TransportFailure>);

    impl Transport for Fake {
        fn post(&self, req: &HttpRequest) -> Result<HttpResponse, TransportFailure> {
            self.0.borrow_mut().push(HttpRequest { url: req.url.clone(), bearer: req.bearer.clone(), body: req.body.clone(), timeout: req.timeout });
            self.1.map(|(status, body)| HttpResponse { status, body: body.into() })
        }
    }

    #[test]
    fn fetch_maps_transport_status_and_body() {
        let key = screened("", "あ", &["a", "b"]);
        let body = r#"{"success":true,"result":{"answers":{"rank":{"choice":"1"}}}}"#;
        let t = Fake(RefCell::default(), Ok((200, body)));
        assert_eq!(fetch_choice(&t, &creds(), &key), Ok(1));
        assert_eq!(t.0.borrow().len(), 1);
        let t = Fake(RefCell::default(), Ok((401, body)));
        assert_eq!(fetch_choice(&t, &creds(), &key), Err(FetchError::Status(401)));
        let t = Fake(RefCell::default(), Err(TransportFailure::Timeout));
        assert_eq!(fetch_choice(&t, &creds(), &key), Err(FetchError::Transport(TransportFailure::Timeout)));
    }

    #[test]
    fn keychain_is_untouched_until_the_first_send() {
        let (mut s, loads) = state(true);
        assert_eq!(s.availability(Instant::now()), Availability::Ready);
        assert_eq!(loads.get(), 0);
        assert!(!s.take_load_request(), "頼まれていないものは読まない");
        s.submit(Instant::now(), job(1, 1, "あ", false));
        s.submit(Instant::now(), job(1, 2, "あい", false));
        assert_eq!(loads.get(), 1, "読み込みは 1 度きりで、結果を使い回す");
    }

    #[test]
    fn a_request_arriving_before_the_load_waits_without_reading() {
        let (mut s, loads) = state(true);
        let t0 = Instant::now();
        let first = s.state.submit(t0, job(1, 1, "あ", false));
        assert_eq!(first, Submitted::default());
        assert_eq!(loads.get(), 0, "submit は store を読まない");
        assert!(s.next_deadline().is_none(), "読み出し待ちは送信待機に入らない");
        assert!(s.state.submit(t0, job(1, 2, "あい", false)).delivery.is_none());
        assert!(s.take_load_request());
        assert!(!s.take_load_request(), "読み出しは 1 度しか頼まれない");
        let loaded = s.state.credentials_loaded(s.store.load());
        assert_eq!(loaded, Loaded::default());
        let due = s.take_all_due(ms(t0, 250));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].generation, 2, "待っている間に来た新しい世代だけが残る");
    }

    #[test]
    fn requests_cancelled_while_loading_are_dropped() {
        let (mut s, _) = state(false);
        let t0 = Instant::now();
        s.state.submit(t0, job(1, 1, "あ", false));
        s.state.submit(t0, job(2, 1, "い", false));
        s.cancel(1);
        assert!(s.take_load_request());
        let loaded = s.state.credentials_loaded(s.store.load());
        assert_eq!(loaded.deliveries.iter().map(|d| d.controller_id).collect::<Vec<_>>(), vec![2]);
        assert!(!s.is_latest(1, 1));
        s.state.submit(t0, job(3, 1, "う", false));
        s.cancel_all();
        assert_eq!(s.pump(), Loaded::default());
    }

    #[test]
    fn missing_credentials_deliver_status_4_and_log_once() {
        let (mut s, loads) = state(false);
        let first = s.submit(Instant::now(), job(1, 1, "あ", false));
        assert_eq!(first.delivery.as_ref().map(|d| d.status), Some(STATUS_CREDENTIALS_MISSING));
        assert!(first.log_missing);
        let second = s.submit(Instant::now(), job(1, 2, "あい", false));
        assert_eq!(second.delivery.as_ref().map(|d| d.status), Some(STATUS_CREDENTIALS_MISSING));
        assert!(!second.log_missing);
        assert_eq!(loads.get(), 1, "打鍵ごとに Keychain を読み直さない");
        assert!(s.next_deadline().is_none());
    }

    #[test]
    fn availability_rereads_only_when_known_missing() {
        let (mut s, loads) = state(false);
        s.submit(Instant::now(), job(1, 1, "あ", false));
        assert_eq!(loads.get(), 1);
        assert_eq!(s.availability(Instant::now()), Availability::CredentialsMissing);
        assert_eq!(loads.get(), 1, "availability は読まない");
        s.request_credentials();
        s.pump();
        assert_eq!(loads.get(), 2);
        assert_eq!(s.availability(Instant::now()), Availability::CredentialsMissing);

        let (mut s, loads) = state(true);
        s.submit(Instant::now(), job(1, 1, "あ", false));
        s.request_credentials();
        assert!(!s.take_load_request(), "読み込み済みなら読み直さない");
        assert_eq!(loads.get(), 1);
    }

    #[test]
    fn credentials_registered_later_are_found_after_a_reload() {
        let (mut s, _) = state(false);
        s.submit(Instant::now(), job(1, 1, "あ", false));
        assert_eq!(s.availability(Instant::now()), Availability::CredentialsMissing);
        *s.store.result.lock().unwrap() = Ok(Some(creds()));
        s.request_credentials();
        assert_eq!(s.availability(Instant::now()), Availability::Ready, "読み出し中は使えるとみなす");
        s.pump();
        let t0 = Instant::now();
        assert_eq!(s.submit(t0, job(1, 2, "あい", true)), Submitted::default());
        assert_eq!(s.take_all_due(t0).len(), 1);
    }

    #[test]
    fn an_unreadable_store_is_unavailable_but_can_be_reread() {
        let (mut s, loads) = rig(Err(StoreError));
        let first = s.submit(Instant::now(), job(1, 1, "あ", false));
        assert_eq!(first.delivery.as_ref().map(|d| d.status), Some(STATUS_CREDENTIALS_MISSING));
        assert!(first.log_missing);
        assert!(!s.submit(Instant::now(), job(1, 2, "あい", false)).log_missing);
        assert_eq!(loads.get(), 1);
        assert_eq!(s.availability(Instant::now()), Availability::CredentialsMissing);
        *s.store.result.lock().unwrap() = Ok(Some(creds()));
        s.request_credentials();
        s.pump();
        assert_eq!(s.availability(Instant::now()), Availability::Ready);
        assert_eq!(loads.get(), 2);
    }

    #[test]
    fn a_single_candidate_is_ignored() {
        let (mut s, loads) = state(true);
        let mut j = job(1, 1, "あ", false);
        j.payload = screened("", "あ", &["今日は"]);
        assert_eq!(s.submit(Instant::now(), j), Submitted::default());
        assert_eq!(loads.get(), 0);
        assert!(s.next_deadline().is_none());
    }

    #[test]
    fn normal_requests_wait_for_the_debounce() {
        let (mut s, _) = state(true);
        let t0 = Instant::now();
        s.submit(t0, job(1, 1, "あ", false));
        assert_eq!(s.next_deadline(), Some(ms(t0, 250)));
        assert!(s.take_all_due(ms(t0, 249)).is_empty());
        let due = s.take_all_due(ms(t0, 250));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].request_number, 1);
        assert!(s.take_all_due(ms(t0, 1000)).is_empty(), "取り出した送信は残らない");
    }

    #[test]
    fn urgent_requests_skip_the_debounce() {
        let (mut s, _) = state(true);
        let t0 = Instant::now();
        s.submit(t0, job(1, 1, "あ", true));
        assert_eq!(s.take_all_due(t0).len(), 1);
    }

    #[test]
    fn a_newer_generation_replaces_the_waiting_one() {
        let (mut s, _) = state(true);
        let t0 = Instant::now();
        s.submit(t0, job(1, 1, "あ", false));
        s.submit(ms(t0, 100), job(1, 2, "あい", false));
        assert!(s.take_all_due(ms(t0, 250)).is_empty(), "新しい世代のデバウンスを待つ");
        let due = s.take_all_due(ms(t0, 350));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].generation, 2);
        assert_eq!(due[0].request_number, 1, "捨てた世代は数えない");
    }

    #[test]
    fn take_due_hands_out_only_free_slots_and_the_rest_waits() {
        let (mut s, _) = state(true);
        let t0 = Instant::now();
        for id in 1..=3 {
            s.submit(t0, job(id, 1, "あ", true));
        }
        assert_eq!(s.take_due(t0).len(), MAX_IN_FLIGHT);
        assert!(!s.has_free_slot());
        assert!(s.take_due(t0).is_empty(), "枠が埋まっている間は取り出さない");
        assert!(s.next_deadline().is_some(), "3 件目は待機に残る");
        s.release_slot();
        let rest = s.take_due(t0);
        assert_eq!(rest.iter().map(|d| d.controller_id).collect::<Vec<_>>(), vec![3]);
        assert!(!s.has_free_slot());
    }

    #[test]
    fn a_disabled_scheduler_accepts_nothing_and_reports_disabled() {
        let (mut s, loads) = state(true);
        let t0 = Instant::now();
        s.submit(t0, job(1, 1, "あ", false));
        assert!(s.disable(), "最初の 1 回だけ true");
        assert!(!s.disable());
        assert_eq!(s.availability(Instant::now()), Availability::Disabled);
        assert!(s.next_deadline().is_none(), "待機中の要求は捨てる");
        assert_eq!(s.submit(t0, job(1, 2, "あい", true)), Submitted::default());
        s.request_credentials();
        assert!(!s.take_load_request());
        assert!(s.next_deadline().is_none(), "ジョブを溜めない");
        assert_eq!(loads.get(), 1);
    }

    #[test]
    fn cancel_forgets_the_controller_and_drops_its_result() {
        let (mut s, _) = state(true);
        let t0 = Instant::now();
        s.submit(t0, job(1, 1, "あ", false));
        s.submit(t0, job(2, 1, "い", false));
        s.cancel(1);
        let due = s.take_all_due(ms(t0, 250));
        assert_eq!(due.iter().map(|d| d.controller_id).collect::<Vec<_>>(), vec![2]);
        s.cancel(2);
        assert_eq!(s.complete(t0, due.into_iter().next().unwrap(), Ok(1)).delivery, None);
        assert!(!s.is_latest(1, 1) && !s.is_latest(2, 1));
    }

    #[test]
    fn controllers_are_independent() {
        let (mut s, _) = state(true);
        let t0 = Instant::now();
        s.submit(t0, job(1, 1, "あ", false));
        s.submit(t0, job(2, 1, "い", false));
        let due = s.take_all_due(ms(t0, 250));
        assert_eq!(due.iter().map(|d| d.controller_id).collect::<Vec<_>>(), vec![1, 2]);
    }

    #[test]
    fn a_result_for_a_superseded_generation_is_dropped() {
        let (mut s, _) = state(true);
        let t0 = Instant::now();
        s.submit(t0, job(1, 1, "あ", false));
        let sent = s.take_all_due(ms(t0, 250)).remove(0);
        s.submit(ms(t0, 300), job(1, 2, "あい", false));
        assert_eq!(s.complete(t0, sent, Ok(1)).delivery, None);
    }

    #[test]
    fn a_successful_result_is_delivered_and_failure_reports_status_1() {
        let (mut s, _) = state(true);
        let t0 = Instant::now();
        s.submit(t0, job(1, 1, "あ", false));
        let sent = s.take_all_due(ms(t0, 250)).remove(0);
        let d = s.complete(t0, sent, Ok(1)).delivery.unwrap();
        assert_eq!((d.status, d.text.as_str(), d.generation), (STATUS_OK, "1", 1));
        s.submit(ms(t0, 300), job(1, 2, "あい", false));
        let sent = s.take_all_due(ms(t0, 600)).remove(0);
        let d = s.complete(t0, sent, Err(FetchError::Transport(TransportFailure::Timeout))).delivery.unwrap();
        assert_eq!(d.status, STATUS_FAILED);
    }

    #[test]
    fn an_identical_request_reuses_the_last_result_without_sending() {
        let (mut s, _) = state(true);
        let t0 = Instant::now();
        s.submit(t0, job(1, 1, "あ", false));
        let sent = s.take_all_due(ms(t0, 250)).remove(0);
        s.complete(t0, sent, Ok(1)).delivery.unwrap();
        let again = s.submit(ms(t0, 400), job(1, 2, "あ", false));
        let d = again.delivery.expect("キャッシュで即座に返る");
        assert_eq!((d.status, d.text.as_str(), d.generation), (STATUS_OK, "1", 2));
        assert!(s.next_deadline().is_none(), "通信は待機に入らない");
    }

    #[test]
    fn a_failed_request_is_not_cached() {
        let (mut s, _) = state(true);
        let t0 = Instant::now();
        s.submit(t0, job(1, 1, "あ", false));
        let sent = s.take_all_due(ms(t0, 250)).remove(0);
        s.complete(t0, sent, Err(FetchError::Status(400)));
        let again = s.submit(ms(t0, 400), job(1, 2, "あ", false));
        assert!(again.delivery.is_none());
        assert!(s.next_deadline().is_some());
    }

    #[test]
    fn only_the_last_result_is_remembered() {
        let (mut s, _) = state(true);
        let t0 = Instant::now();
        for (generation, reading) in [(1, "あ"), (2, "い")] {
            s.submit(t0, job(1, generation, reading, true));
            let sent = s.take_all_due(t0).remove(0);
            s.complete(t0, sent, Ok(0)).delivery.unwrap();
        }
        assert!(s.submit(t0, job(1, 3, "あ", true)).delivery.is_none());
    }

    #[test]
    fn urgent_for_an_already_sent_generation_does_nothing() {
        let (mut s, _) = state(true);
        let t0 = Instant::now();
        s.submit(t0, job(1, 1, "あ", false));
        s.take_all_due(ms(t0, 250));
        let again = s.submit(ms(t0, 260), job(1, 1, "あ", true));
        assert_eq!(again, Submitted::default());
        assert!(s.next_deadline().is_none(), "二重送信しない");
    }

    #[test]
    fn request_numbers_count_sent_requests() {
        let (mut s, _) = state(true);
        let t0 = Instant::now();
        for generation in 1..=3 {
            s.submit(t0, job(1, generation, &format!("あ{generation}"), true));
            let sent = s.take_all_due(t0).remove(0);
            assert_eq!(sent.request_number, generation);
        }
    }

    fn fail(s: &mut Rig, t0: Instant, id: u64, generation: u64, reading: &str, err: FetchError) -> Completed {
        s.submit(t0, job(id, generation, reading, true));
        let sent = s.take_all_due(t0).remove(0);
        s.complete(t0, sent, Err(err))
    }

    #[test]
    fn failures_are_classified_by_how_to_respond() {
        use FailureClass::*;
        let cases = [
            (FetchError::Transport(TransportFailure::Timeout), Transient),
            (FetchError::Transport(TransportFailure::Connect), Transient),
            (FetchError::Transport(TransportFailure::Tls), Transient),
            (FetchError::Transport(TransportFailure::Read), Transient),
            (FetchError::Status(401), AuthRejected),
            (FetchError::Status(403), AuthRejected),
            (FetchError::Status(429), Backoff),
            (FetchError::Status(500), Backoff),
            (FetchError::Status(503), Backoff),
            (FetchError::Status(400), Permanent),
            (FetchError::Status(404), Permanent),
            (FetchError::InvalidResponse, Permanent),
        ];
        for (err, class) in cases {
            assert_eq!(err.classify(), class, "{err:?}");
        }
    }

    #[test]
    fn a_rejected_credential_stops_sending_until_credentials_are_requested_again() {
        for code in [401, 403] {
            let (mut s, loads) = state(true);
            let t0 = Instant::now();
            s.submit(t0, job(1, 1, "あ", false));
            s.submit(t0, job(2, 1, "い", false));
            let sent = s.take_all_due(ms(t0, 250)).remove(0);
            let done = s.complete(t0, sent, Err(FetchError::Status(code)));
            assert_eq!(done.delivery, None, "{code}");
            let log = done.log.expect("状態が変わったので 1 度ログを出す");
            assert!(log.contains(&code.to_string()), "{log}");
            assert_eq!(s.availability(t0), Availability::CredentialsRejected, "{code}");
            assert!(s.next_deadline().is_none(), "待機中のものは捨てる: {code}");
            assert_eq!(s.submit(t0, job(1, 2, "あい", true)), Submitted::default());
            assert!(s.next_deadline().is_none(), "拒否されている間は送信待機に入れない: {code}");
            assert!(!s.take_load_request(), "勝手に読み直さない");
            assert_eq!(loads.get(), 1);
            s.request_credentials();
            s.pump();
            assert_eq!(loads.get(), 2, "トグルを入れ直したら認証情報を読み直す");
            assert_eq!(s.availability(t0), Availability::Ready);
            s.submit(t0, job(1, 3, "あいう", true));
            assert_eq!(s.take_all_due(t0).len(), 1, "再開する");
        }
    }

    #[test]
    fn a_second_rejection_does_not_log_again() {
        let (mut s, _) = state(true);
        let t0 = Instant::now();
        s.submit(t0, job(1, 1, "あ", true));
        s.submit(t0, job(2, 1, "い", true));
        let mut due = s.take_all_due(t0);
        let (b, a) = (due.remove(1), due.remove(0));
        assert!(s.complete(t0, a, Err(FetchError::Status(401))).log.is_some());
        assert_eq!(s.complete(t0, b, Err(FetchError::Status(401))), Completed::default());
    }

    #[test]
    fn rate_limits_and_server_errors_pause_for_a_minute_then_resume() {
        for code in [429, 500, 503] {
            let (mut s, _) = state(true);
            let t0 = Instant::now();
            s.submit(t0, job(2, 1, "い", false));
            let done = fail(&mut s, t0, 1, 1, "あ", FetchError::Status(code));
            assert_eq!(done.delivery, None, "{code}");
            assert!(done.log.expect("状態が変わったので 1 度ログを出す").contains(&code.to_string()));
            assert_eq!(s.availability(t0), Availability::Paused, "{code}");
            assert!(s.next_deadline().is_none(), "待機中のものは捨てる: {code}");
            assert_eq!(s.submit(ms(t0, 1000), job(1, 2, "あい", true)), Submitted::default());
            assert!(s.next_deadline().is_none(), "止めている間は受け付けない: {code}");
            assert_eq!(s.availability(t0 + Duration::from_secs(59)), Availability::Paused);
            assert_eq!(s.availability(t0 + Duration::from_secs(60)), Availability::Ready, "{code}");
            let t1 = t0 + Duration::from_secs(60);
            s.submit(t1, job(1, 3, "あいう", true));
            assert_eq!(s.take_all_due(t1).len(), 1, "60 秒たてば送る");
        }
    }

    #[test]
    fn pausing_again_while_paused_does_not_log_again() {
        let (mut s, _) = state(true);
        let t0 = Instant::now();
        s.submit(t0, job(1, 1, "あ", true));
        s.submit(t0, job(2, 1, "い", true));
        let mut due = s.take_all_due(t0);
        let (b, a) = (due.remove(1), due.remove(0));
        assert!(s.complete(t0, a, Err(FetchError::Status(429))).log.is_some());
        let again = s.complete(ms(t0, 500), b, Err(FetchError::Status(503)));
        assert_eq!(again.log, None);
        assert_eq!(s.availability(t0 + Duration::from_secs(60)), Availability::Ready, "停止期間を延ばさない");
    }

    #[test]
    fn a_transient_failure_fails_only_that_request() {
        for failure in [TransportFailure::Timeout, TransportFailure::Connect, TransportFailure::Tls, TransportFailure::Read] {
            let (mut s, _) = state(true);
            let t0 = Instant::now();
            let done = fail(&mut s, t0, 1, 1, "あ", FetchError::Transport(failure));
            assert_eq!(done.delivery.map(|d| d.status), Some(STATUS_FAILED));
            assert_eq!(done.log, None);
            assert_eq!(s.availability(t0), Availability::Ready);
            assert!(s.next_deadline().is_none(), "再送しない");
            s.submit(t0, job(1, 2, "あい", true));
            assert_eq!(s.take_all_due(t0).len(), 1, "次の打鍵は普通に送る");
        }
    }

    #[test]
    fn three_permanent_failures_in_a_row_disable_and_a_success_resets_the_count() {
        let (mut s, _) = state(true);
        let t0 = Instant::now();
        let errors = [FetchError::Status(400), FetchError::InvalidResponse, FetchError::Status(404)];
        for (n, err) in errors.iter().enumerate().take(2) {
            let done = fail(&mut s, t0, 1, n as u64 + 1, &format!("あ{n}"), err.clone());
            assert_eq!(done.delivery.map(|d| d.status), Some(STATUS_FAILED));
            assert_eq!(done.log, None);
            assert_eq!(s.availability(t0), Availability::Ready);
        }
        s.submit(t0, job(1, 3, "ok", true));
        let sent = s.take_all_due(t0).remove(0);
        s.complete(t0, sent, Ok(0)).delivery.unwrap();
        for (n, err) in errors.iter().enumerate().take(2) {
            fail(&mut s, t0, 1, n as u64 + 10, &format!("い{n}"), err.clone());
            assert_eq!(s.availability(t0), Availability::Ready, "成功で数え直した");
        }
        let done = fail(&mut s, t0, 1, 20, "う", errors[2].clone());
        assert!(done.log.expect("無効化したとき 1 度だけログ").contains("disabled"));
        assert_eq!(s.availability(t0), Availability::Disabled);
        assert!(s.next_deadline().is_none());
        s.request_credentials();
        assert!(!s.take_load_request(), "トグルでは戻らない");
        assert_eq!(s.submit(t0, job(1, 30, "え", true)), Submitted::default());
    }
}
