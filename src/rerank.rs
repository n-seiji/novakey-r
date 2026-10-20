//! AI 並べ替え (Cloudflare Workers AI の clef-flash) の判断部分。入出力を持たない純粋な状態機械。
//!
//! 通信と Keychain は trait (`Transport` / `CredentialStore`)、時刻は引数 (`now`) で渡すので、
//! デバウンス・最新世代だけを採用する規則・同一リクエストの再利用は、ネットワークなしでテストできる。
//! 呼び出し側 (`llm_bridge`) は `State` を Mutex で包み、`take_due` で取り出した送信をロックの外で実行して `complete` に返す。
#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};

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

/// Keychain 等から認証情報を読む。未登録は `None`。
pub trait CredentialStore {
    fn load(&self) -> Option<Credentials>;
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

/// 同一リクエストの判定キー。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RerankKey {
    pub context: String,
    pub reading: String,
    pub candidates: Vec<String>,
}

pub fn build_request(creds: &Credentials, key: &RerankKey) -> HttpRequest {
    let criteria: serde_json::Map<String, serde_json::Value> = key
        .candidates
        .iter()
        .enumerate()
        .map(|(i, c)| (i.to_string(), serde_json::Value::String(c.clone())))
        .collect();
    let mut state = serde_json::Map::new();
    state.insert("reading".into(), key.reading.clone().into());
    if !key.context.is_empty() {
        state.insert("context".into(), key.context.clone().into());
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

pub fn fetch_choice(transport: &dyn Transport, creds: &Credentials, key: &RerankKey) -> Result<usize, FetchError> {
    let response = transport.post(&build_request(creds, key)).map_err(|_| FetchError::Transport)?;
    if response.status != 200 {
        return Err(FetchError::Status(response.status));
    }
    parse_choice(&response.body, key.candidates.len())
}

pub struct Job {
    pub controller_id: u64,
    pub generation: u64,
    pub reading: String,
    pub context: String,
    pub candidates: Vec<String>,
    /// Space 押下時。デバウンスを待たずに送る。
    pub urgent: bool,
}

struct Pending {
    due: Instant,
    generation: u64,
    key: RerankKey,
    creds: Credentials,
}

/// `take_due` が取り出した、これから送る 1 件。
pub struct InFlight {
    pub controller_id: u64,
    pub generation: u64,
    /// 送信の通し番号 ("Clef requests sent: N")。
    pub request_number: u64,
    pub key: RerankKey,
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

pub struct State<S: CredentialStore> {
    store: S,
    /// `None` は未読込、`Some(None)` は読んだが未登録。Keychain は最初の送信まで触らない。
    creds: Option<Option<Credentials>>,
    missing_logged: bool,
    latest: HashMap<u64, u64>,
    /// 送信を始めた (または結果を配信した) 世代。urgent の重複送信を防ぐ。
    sent: HashMap<u64, u64>,
    /// 直近 1 件の成功結果。同一リクエストは通信せずこれを返す。
    last_result: Option<(RerankKey, usize)>,
    request_count: u64,
    pending: HashMap<u64, Pending>,
}

impl<S: CredentialStore> State<S> {
    pub fn new(store: S) -> Self {
        Self {
            store,
            creds: None,
            missing_logged: false,
            latest: HashMap::new(),
            sent: HashMap::new(),
            last_result: None,
            request_count: 0,
            pending: HashMap::new(),
        }
    }

    /// トグルを ON にしたときの確認。まだ読んでいなければ Keychain には触れず「使える」とみなす
    /// (最初の送信で分かる)。未登録と分かっているときだけ読み直す。
    pub fn availability(&mut self) -> bool {
        if matches!(self.creds, Some(None)) {
            self.creds = Some(self.store.load());
        }
        !matches!(self.creds, Some(None))
    }

    fn credentials(&mut self) -> Option<Credentials> {
        if self.creds.is_none() {
            self.creds = Some(self.store.load());
        }
        self.creds.clone().flatten()
    }

    fn register_latest(&mut self, id: u64, generation: u64) {
        self.latest.insert(id, generation);
        self.pending.remove(&id);
    }

    pub fn is_latest(&self, id: u64, generation: u64) -> bool {
        self.latest.get(&id) == Some(&generation)
    }

    pub fn submit(&mut self, now: Instant, job: Job) -> Submitted {
        let Job { controller_id: id, generation, reading, context, candidates, urgent } = job;
        if candidates.len() < 2 {
            return Submitted::default();
        }
        let Some(creds) = self.credentials() else {
            let log_missing = !self.missing_logged;
            self.missing_logged = true;
            self.register_latest(id, generation);
            return Submitted {
                delivery: Some(Delivery {
                    controller_id: id,
                    generation,
                    status: STATUS_CREDENTIALS_MISSING,
                    text: "Cloudflare認証情報なし".into(),
                }),
                log_missing,
            };
        };
        let key = RerankKey { context, reading, candidates };
        if let Some((cached_key, index)) = &self.last_result {
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
    pub fn take_due(&mut self, now: Instant) -> Vec<InFlight> {
        let mut ids: Vec<u64> = self.pending.iter().filter(|(_, p)| p.due <= now).map(|(id, _)| *id).collect();
        ids.sort_unstable();
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
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    struct Store {
        creds: Option<Credentials>,
        loads: Rc<Cell<u32>>,
    }

    impl CredentialStore for Store {
        fn load(&self) -> Option<Credentials> {
            self.loads.set(self.loads.get() + 1);
            self.creds.clone()
        }
    }

    fn creds() -> Credentials {
        Credentials { api_token: "tok".into(), account_id: "acct".into() }
    }

    fn state(with_creds: bool) -> (State<Store>, Rc<Cell<u32>>) {
        let loads = Rc::new(Cell::new(0));
        let store = Store { creds: with_creds.then(creds), loads: loads.clone() };
        (State::new(store), loads)
    }

    fn job(id: u64, generation: u64, reading: &str, urgent: bool) -> Job {
        Job {
            controller_id: id,
            generation,
            reading: reading.into(),
            context: String::new(),
            candidates: vec!["今日は".into(), "京は".into()],
            urgent,
        }
    }

    fn ms(base: Instant, n: u64) -> Instant {
        base + Duration::from_millis(n)
    }

    #[test]
    fn request_has_endpoint_bearer_and_body() {
        let key = RerankKey { context: "前文".into(), reading: "きょうは".into(), candidates: vec!["a".into(), "b".into()] };
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
        let key = RerankKey { context: String::new(), reading: "あ".into(), candidates: vec!["a".into(), "b".into()] };
        let body: serde_json::Value = serde_json::from_str(&build_request(&creds(), &key).body).unwrap();
        assert!(body["state"].get("context").is_none());
    }

    #[test]
    fn debug_output_hides_secrets() {
        let key = RerankKey { context: String::new(), reading: "あ".into(), candidates: vec![] };
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
        let key = RerankKey { context: String::new(), reading: "あ".into(), candidates: vec!["a".into(), "b".into()] };
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
        s.submit(Instant::now(), job(1, 1, "あ", false));
        s.submit(Instant::now(), job(1, 2, "あい", false));
        assert_eq!(loads.get(), 1, "読み込みは 1 度きりで、結果を使い回す");
    }

    #[test]
    fn missing_credentials_deliver_status_4_and_log_once() {
        let (mut s, loads) = state(false);
        let first = s.submit(Instant::now(), job(1, 1, "あ", false));
        assert_eq!(first.delivery.as_ref().map(|d| d.status), Some(STATUS_CREDENTIALS_MISSING));
        assert!(first.log_missing);
        let second = s.submit(Instant::now(), job(1, 2, "あい", false));
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
        assert_eq!(loads.get(), 2);
    }

    #[test]
    fn a_single_candidate_is_ignored() {
        let (mut s, loads) = state(true);
        let mut j = job(1, 1, "あ", false);
        j.candidates.truncate(1);
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
