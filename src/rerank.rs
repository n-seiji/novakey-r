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

/// POST を 1 回送る。エラー文字列は URL (アカウント ID を含む) を含みうるので、ログに出さない。
pub trait Transport {
    fn post(&self, req: &HttpRequest) -> Result<HttpResponse, String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    Transport,
    Status(u16),
    InvalidResponse,
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
    let response = transport.post(&build_request(creds, key)).map_err(|_| FetchError::Transport)?;
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
        }
    }

    /// 使えなさそうか。未読込・読み出し中は「使える」とみなす (送信時に分かる)。読まない。
    pub fn availability(&self) -> bool {
        !matches!(self.creds, CredState::Missing | CredState::Unreadable)
    }

    /// 認証情報の読み出しを頼む (トグル ON 時)。読み込み済みなら何もしない。
    /// 未登録・読めなかったときは読み直す。実際に読むのはワーカー。
    pub fn request_credentials(&mut self) {
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

    /// 時刻が来た送信を取り出す。取り出した時点で「送信済み」として数える。
    #[cfg(test)]
    pub fn take_due(&mut self, now: Instant) -> Vec<InFlight> {
        self.take_due_up_to(now, usize::MAX)
    }

    /// 取り出す件数に上限をつけた版。残りは待機に残り、次の呼び出しで取り出す。
    pub fn take_due_up_to(&mut self, now: Instant, max: usize) -> Vec<InFlight> {
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

    /// 通信の結果を受ける。その間に新しい世代が来ていれば捨てる (最新だけ採用)。
    pub fn complete(&mut self, sent: InFlight, result: Result<usize, FetchError>) -> Option<Delivery> {
        if !self.is_latest(sent.controller_id, sent.generation) {
            return None;
        }
        let (status, text) = match result {
            Ok(index) => {
                self.last_result = Some((sent.key, index));
                (STATUS_OK, index.to_string())
            }
            Err(_) => (STATUS_FAILED, "Clef rerank failed".to_string()),
        };
        Some(Delivery { controller_id: sent.controller_id, generation: sent.generation, status, text })
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

    struct Fake(RefCell<Vec<HttpRequest>>, Result<(u16, &'static str), &'static str>);

    impl Transport for Fake {
        fn post(&self, req: &HttpRequest) -> Result<HttpResponse, String> {
            self.0.borrow_mut().push(HttpRequest { url: req.url.clone(), bearer: req.bearer.clone(), body: req.body.clone(), timeout: req.timeout });
            self.1.map(|(status, body)| HttpResponse { status, body: body.into() }).map_err(String::from)
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
        let t = Fake(RefCell::default(), Err("https://secret-url timed out"));
        assert_eq!(fetch_choice(&t, &creds(), &key), Err(FetchError::Transport));
    }

    #[test]
    fn keychain_is_untouched_until_the_first_send() {
        let (mut s, loads) = state(true);
        assert!(s.availability());
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
        let due = s.take_due(ms(t0, 250));
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
        assert!(!s.availability());
        assert_eq!(loads.get(), 1, "availability は読まない");
        s.request_credentials();
        s.pump();
        assert_eq!(loads.get(), 2);
        assert!(!s.availability());

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
        assert!(!s.availability());
        *s.store.result.lock().unwrap() = Ok(Some(creds()));
        s.request_credentials();
        assert!(s.availability(), "読み出し中は使えるとみなす");
        s.pump();
        let t0 = Instant::now();
        assert_eq!(s.submit(t0, job(1, 2, "あい", true)), Submitted::default());
        assert_eq!(s.take_due(t0).len(), 1);
    }

    #[test]
    fn an_unreadable_store_is_unavailable_but_can_be_reread() {
        let (mut s, loads) = rig(Err(StoreError));
        let first = s.submit(Instant::now(), job(1, 1, "あ", false));
        assert_eq!(first.delivery.as_ref().map(|d| d.status), Some(STATUS_CREDENTIALS_MISSING));
        assert!(first.log_missing);
        assert!(!s.submit(Instant::now(), job(1, 2, "あい", false)).log_missing);
        assert_eq!(loads.get(), 1);
        assert!(!s.availability());
        *s.store.result.lock().unwrap() = Ok(Some(creds()));
        s.request_credentials();
        s.pump();
        assert!(s.availability());
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
        assert!(s.take_due(ms(t0, 249)).is_empty());
        let due = s.take_due(ms(t0, 250));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].request_number, 1);
        assert!(s.take_due(ms(t0, 1000)).is_empty(), "取り出した送信は残らない");
    }

    #[test]
    fn urgent_requests_skip_the_debounce() {
        let (mut s, _) = state(true);
        let t0 = Instant::now();
        s.submit(t0, job(1, 1, "あ", true));
        assert_eq!(s.take_due(t0).len(), 1);
    }

    #[test]
    fn a_newer_generation_replaces_the_waiting_one() {
        let (mut s, _) = state(true);
        let t0 = Instant::now();
        s.submit(t0, job(1, 1, "あ", false));
        s.submit(ms(t0, 100), job(1, 2, "あい", false));
        assert!(s.take_due(ms(t0, 250)).is_empty(), "新しい世代のデバウンスを待つ");
        let due = s.take_due(ms(t0, 350));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].generation, 2);
        assert_eq!(due[0].request_number, 1, "捨てた世代は数えない");
    }

    #[test]
    fn take_due_up_to_leaves_the_rest_waiting() {
        let (mut s, _) = state(true);
        let t0 = Instant::now();
        for id in 1..=3 {
            s.submit(t0, job(id, 1, "あ", true));
        }
        assert_eq!(s.take_due_up_to(t0, 2).len(), 2);
        let rest = s.take_due_up_to(t0, 2);
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].controller_id, 3);
    }

    #[test]
    fn cancel_forgets_the_controller_and_drops_its_result() {
        let (mut s, _) = state(true);
        let t0 = Instant::now();
        s.submit(t0, job(1, 1, "あ", false));
        s.submit(t0, job(2, 1, "い", false));
        s.cancel(1);
        let due = s.take_due(ms(t0, 250));
        assert_eq!(due.iter().map(|d| d.controller_id).collect::<Vec<_>>(), vec![2]);
        s.cancel(2);
        assert_eq!(s.complete(due.into_iter().next().unwrap(), Ok(1)), None);
        assert!(!s.is_latest(1, 1) && !s.is_latest(2, 1));
    }

    #[test]
    fn controllers_are_independent() {
        let (mut s, _) = state(true);
        let t0 = Instant::now();
        s.submit(t0, job(1, 1, "あ", false));
        s.submit(t0, job(2, 1, "い", false));
        let due = s.take_due(ms(t0, 250));
        assert_eq!(due.iter().map(|d| d.controller_id).collect::<Vec<_>>(), vec![1, 2]);
    }

    #[test]
    fn a_result_for_a_superseded_generation_is_dropped() {
        let (mut s, _) = state(true);
        let t0 = Instant::now();
        s.submit(t0, job(1, 1, "あ", false));
        let sent = s.take_due(ms(t0, 250)).remove(0);
        s.submit(ms(t0, 300), job(1, 2, "あい", false));
        assert_eq!(s.complete(sent, Ok(1)), None);
    }

    #[test]
    fn a_successful_result_is_delivered_and_failure_reports_status_1() {
        let (mut s, _) = state(true);
        let t0 = Instant::now();
        s.submit(t0, job(1, 1, "あ", false));
        let sent = s.take_due(ms(t0, 250)).remove(0);
        let d = s.complete(sent, Ok(1)).unwrap();
        assert_eq!((d.status, d.text.as_str(), d.generation), (STATUS_OK, "1", 1));
        s.submit(ms(t0, 300), job(1, 2, "あい", false));
        let sent = s.take_due(ms(t0, 600)).remove(0);
        let d = s.complete(sent, Err(FetchError::Transport)).unwrap();
        assert_eq!(d.status, STATUS_FAILED);
    }

    #[test]
    fn an_identical_request_reuses_the_last_result_without_sending() {
        let (mut s, _) = state(true);
        let t0 = Instant::now();
        s.submit(t0, job(1, 1, "あ", false));
        let sent = s.take_due(ms(t0, 250)).remove(0);
        s.complete(sent, Ok(1)).unwrap();
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
        let sent = s.take_due(ms(t0, 250)).remove(0);
        s.complete(sent, Err(FetchError::Status(500)));
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
            let sent = s.take_due(t0).remove(0);
            s.complete(sent, Ok(0)).unwrap();
        }
        assert!(s.submit(t0, job(1, 3, "あ", true)).delivery.is_none());
    }

    #[test]
    fn urgent_for_an_already_sent_generation_does_nothing() {
        let (mut s, _) = state(true);
        let t0 = Instant::now();
        s.submit(t0, job(1, 1, "あ", false));
        s.take_due(ms(t0, 250));
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
            let sent = s.take_due(t0).remove(0);
            assert_eq!(sent.request_number, generation);
        }
    }
}
