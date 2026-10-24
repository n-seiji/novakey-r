#![forbid(unsafe_code)]

//! AI 並べ替えの送信ゲート。Cocoa には触れない。先に送信元アプリを確かめ、通ったときだけ直前テキストを読んで検査する。
//! 拒否するアプリでは、ホストへ直前テキストを問い合わせること自体をしない。

use std::cell::Cell;

use crate::controller::{Environment, TextClient};
use crate::outbound::{self, Decision, Screened};
use crate::rerank::Availability;

/// 直前文脈として読む最大長 (UTF-16 単位)。
const CONTEXT_MAX_UTF16: usize = 200;

/// AI 並べ替えを使えるか (main スレッド側の唯一の記録)。トグルと、送信側から分かった状態をまとめる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiStatus {
    Off,
    On(Availability),
}

impl AiStatus {
    pub fn is_on(self) -> bool {
        !matches!(self, AiStatus::Off)
    }

    /// 要求を送ってよいか。送れないときは送信側 (Keychain を読む経路) へ進まない。
    pub fn allows_requests(self) -> bool {
        self == AiStatus::On(Availability::Ready)
    }

    /// オフにした後に届いた結果は、取り消し前に配信が済んでいたものでも反映しない。
    pub fn accepts_results(self) -> bool {
        self.is_on()
    }

    /// 送信側から分かった状態を取り込む。オフのままなら何もしない。
    pub fn observe(self, availability: Availability) -> Self {
        match self {
            AiStatus::Off => AiStatus::Off,
            AiStatus::On(_) => AiStatus::On(availability),
        }
    }

    /// メニューに出す警告。使えるとき・オフのときは出さない。
    pub fn warning(self) -> Option<&'static str> {
        match self {
            AiStatus::Off | AiStatus::On(Availability::Ready) => None,
            AiStatus::On(Availability::CredentialsMissing) => Some("LLMリランキング無効(Cloudflare認証情報を確認)"),
            AiStatus::On(Availability::CredentialsRejected) => Some("LLMリランキング無効(Cloudflare認証情報が拒否されました)"),
            AiStatus::On(Availability::Paused) => Some("LLMリランキング一時停止中"),
            AiStatus::On(Availability::Disabled) => Some("LLMリランキング停止(内部エラー。再起動で復帰)"),
        }
    }
}

pub enum GateOutcome {
    Send(Screened),
    /// 送らない。デバウンス中の古い要求も取り消すこと。
    Cancel,
}

#[derive(Default)]
pub struct RerankGate {
    /// 直前に外へ出した「送らなかった / 文脈なしで送った」理由。同じ理由を打鍵ごとにログしないために持つ。
    last: Cell<Option<(&'static str, &'static str)>>,
}

impl RerankGate {
    pub fn check<E: Environment, C: TextClient>(
        &self,
        env: &E,
        bundle_id: Option<&str>,
        client: Option<&C>,
        reading: &str,
        candidates: &[String],
    ) -> GateOutcome {
        let pass = match outbound::gate_app(env.secure_input(), bundle_id) {
            Ok(pass) => pass,
            Err(reason) => {
                self.log(env, Some(("skipped", reason.name())));
                return GateOutcome::Cancel;
            }
        };
        let preceding = client.map(|c| c.preceding_text(CONTEXT_MAX_UTF16)).unwrap_or_default();
        match outbound::screen(pass, reading, candidates, &preceding) {
            Decision::Skip(reason) => {
                self.log(env, Some(("skipped", reason.name())));
                GateOutcome::Cancel
            }
            Decision::Send(screened) => {
                self.log(env, screened.context_dropped().map(|r| ("without context", r.name())));
                GateOutcome::Send(screened)
            }
        }
    }

    /// 理由が前回と変わったときだけログする。名前だけで入力は含まない。
    fn log<E: Environment>(&self, env: &E, gate: Option<(&'static str, &'static str)>) {
        if self.last.replace(gate) != gate {
            if let Some((kind, reason)) = gate {
                env.log(&format!("LLM rerank {kind}: {reason}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Env {
        secure: bool,
        logs: RefCell<Vec<String>>,
    }

    impl Environment for Env {
        fn secure_input(&self) -> bool {
            self.secure
        }
        fn rerank_allowed(&self) -> bool {
            true
        }
        fn log(&self, line: &str) {
            self.logs.borrow_mut().push(line.to_string());
        }
        fn trace(&self, _entry: &str, _fields: &str) {}
        fn send_rerank(&self, _: u64, _: u64, _: &Screened, _: bool) {}
        fn cancel_rerank(&self, _: u64) {}
    }

    struct Client {
        text: String,
        reads: Cell<usize>,
    }

    impl Client {
        fn new(text: &str) -> Self {
            Self { text: text.to_string(), reads: Cell::new(0) }
        }
    }

    impl TextClient for Client {
        fn insert(&self, _: &str) {}
        fn set_marked(&self, _: &str) {}
        fn preceding_text(&self, _: usize) -> String {
            self.reads.set(self.reads.get() + 1);
            self.text.clone()
        }
        fn bundle_id(&self) -> Option<String> {
            None
        }
    }

    const OK_APP: Option<&str> = Some("com.apple.TextEdit");

    fn candidates() -> Vec<String> {
        vec!["今日".to_string()]
    }

    #[test]
    fn 拒否するアプリでは直前テキストを読まず_取り消す() {
        for (secure, bundle) in [(true, OK_APP), (false, Some("com.apple.Terminal")), (false, Some("com.1password.1password")), (false, None)] {
            let (env, client, gate) = (Env { secure, ..Env::default() }, Client::new("今日は"), RerankGate::default());
            let out = gate.check(&env, bundle, Some(&client), "きょう", &candidates());
            assert!(matches!(out, GateOutcome::Cancel), "{secure} {bundle:?}");
            assert_eq!(client.reads.get(), 0, "{secure} {bundle:?}");
        }
    }

    #[test]
    fn 読みか候補が秘密に当たると取り消す() {
        let (env, client, gate) = (Env::default(), Client::new(""), RerankGate::default());
        let out = gate.check(&env, OK_APP, Some(&client), "きょう", &["sk-abcdefghijklmnopqrstuvwxyz0123456789".to_string()]);
        assert!(matches!(out, GateOutcome::Cancel));
    }

    #[test]
    fn 文脈だけが秘密に当たると文脈を空にして送る() {
        let (env, client, gate) = (Env::default(), Client::new("パスワード\nabc"), RerankGate::default());
        let GateOutcome::Send(s) = gate.check(&env, OK_APP, Some(&client), "きょう", &candidates()) else { panic!("送られなかった") };
        assert!(s.context_dropped().is_some());
        assert_eq!(client.reads.get(), 1);
        assert_eq!(env.logs.borrow().len(), 1);
    }

    #[test]
    fn クライアントがなくても文脈なしで送る() {
        let (env, gate) = (Env::default(), RerankGate::default());
        assert!(matches!(gate.check::<Env, Client>(&env, OK_APP, None, "きょう", &candidates()), GateOutcome::Send(_)));
    }

    #[test]
    fn 同じ理由は続けてログしない() {
        let (env, client, gate) = (Env::default(), Client::new(""), RerankGate::default());
        for _ in 0..3 {
            gate.check(&env, None, Some(&client), "きょう", &candidates());
        }
        assert_eq!(env.logs.borrow().len(), 1);
        gate.check(&env, Some("com.apple.Terminal"), Some(&client), "きょう", &candidates());
        assert_eq!(env.logs.borrow().len(), 2);
    }

    #[test]
    fn オフのあいだは要求も結果も通さず状態も取り込まない() {
        let off = AiStatus::Off;
        assert!(!off.allows_requests() && !off.accepts_results());
        assert_eq!(off.observe(Availability::Disabled), AiStatus::Off);
        assert_eq!(off.warning(), None);
    }

    #[test]
    fn 要求を送れるのはオンで使えるときだけ() {
        for a in [Availability::CredentialsMissing, Availability::CredentialsRejected, Availability::Paused, Availability::Disabled] {
            let status = AiStatus::On(Availability::Ready).observe(a);
            assert!(!status.allows_requests(), "{a:?}");
            assert!(status.accepts_results(), "止まっていても、すでに届いた結果は受ける: {a:?}");
            assert!(status.warning().is_some(), "{a:?}");
        }
        assert!(AiStatus::On(Availability::Ready).allows_requests());
        assert_eq!(AiStatus::On(Availability::Ready).warning(), None);
    }

    #[test]
    fn 無効化された状態はメニューの警告に出る() {
        assert!(AiStatus::On(Availability::Disabled).warning().is_some());
    }
}
