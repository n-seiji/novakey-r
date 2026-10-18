#![forbid(unsafe_code)]
//! 外部 (Cloudflare) へ送る前のプライバシーガード。純粋関数と const テーブルだけで構成する。
use std::sync::LazyLock;

use regex::Regex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    SecureInput,
    DeniedApp,
    SecretPattern(&'static str),
}

impl Reason {
    /// ログ用の名前。入力内容は含まない。
    pub fn name(&self) -> &'static str {
        match self {
            Reason::SecureInput => "secure_input",
            Reason::DeniedApp => "denied_app",
            Reason::SecretPattern(name) => name,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outbound {
    Skip(Reason),
    WithoutContext(Reason),
    WithContext(String),
}

pub struct Snapshot<'a> {
    pub secure_input: bool,
    pub bundle_id: Option<&'a str>,
    pub preceding: &'a str,
}

const DENIED_APPS: &[&str] = &[
    "com.1password.1password",
    "com.agilebits.onepassword7",
    "com.bitwarden.desktop",
    "com.apple.keychainaccess",
    "com.apple.Passwords",
    "com.apple.Terminal",
    "com.googlecode.iterm2",
    "dev.warp.Warp-Stable",
    "com.mitchellh.ghostty",
    "net.kovidgoyal.kitty",
    "org.alacritty",
];

const SENTENCE_ENDS: &[char] = &['。', '．', '！', '？', '!', '?', '\n'];
const CONTEXT_MAX_CHARS: usize = 100;

struct Pattern {
    name: &'static str,
    regex: &'static str,
    /// regex に一致した断片をさらに絞り込む述語。regex だけで表せない条件用。
    accept: fn(&str) -> bool,
}

const fn any_match(_: &str) -> bool {
    true
}

fn has_letter_and_digit(s: &str) -> bool {
    s.chars().any(|c| c.is_ascii_alphabetic()) && s.chars().any(|c| c.is_ascii_digit())
}

const SECRET_PATTERNS: &[Pattern] = &[
    Pattern {
        name: "api_key_prefix",
        regex: r"(sk-|sk_live_|pk_live_|ghp_|gho_|ghs_|github_pat_|xox[abposr]-|AKIA|ASIA|AIza|cfut_|glpat-)[A-Za-z0-9_\-]{8,}",
        accept: any_match,
    },
    Pattern { name: "private_key", regex: r"-----BEGIN", accept: any_match },
    Pattern {
        name: "jwt",
        regex: r"eyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}\.",
        accept: any_match,
    },
    Pattern {
        name: "email",
        regex: r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}",
        accept: any_match,
    },
    Pattern { name: "long_digits", regex: r"\d[\d \-]{10,}\d", accept: any_match },
    Pattern {
        name: "high_entropy_token",
        regex: r"[A-Za-z0-9_\-+/=]{24,}",
        accept: has_letter_and_digit,
    },
    Pattern {
        name: "secret_word",
        regex: r"(?i)(password|passwd|passcode|secret|token|api[ _-]?key)|パスワード|暗証番号|合言葉|秘密鍵|認証コード|ワンタイム",
        accept: any_match,
    },
];

static COMPILED: LazyLock<Vec<(&'static Pattern, Regex)>> = LazyLock::new(|| {
    SECRET_PATTERNS
        .iter()
        .map(|p| (p, Regex::new(p.regex).expect("SECRET_PATTERNS の正規表現は有効")))
        .collect()
});

/// 直近の文 (最後の文末記号より後ろ) の末尾 CONTEXT_MAX_CHARS 文字を trim して返す。
fn current_sentence(preceding: &str) -> &str {
    let tail = preceding.rsplit(SENTENCE_ENDS).next().unwrap_or("");
    let skip = tail.chars().count().saturating_sub(CONTEXT_MAX_CHARS);
    tail.char_indices()
        .nth(skip)
        .map_or("", |(start, _)| &tail[start..])
        .trim()
}

fn find_secret(text: &str) -> Option<&'static str> {
    COMPILED
        .iter()
        .find(|(p, re)| re.find_iter(text).any(|m| (p.accept)(m.as_str())))
        .map(|(p, _)| p.name)
}

pub fn decide(s: &Snapshot) -> Outbound {
    if s.secure_input {
        return Outbound::Skip(Reason::SecureInput);
    }
    if s.bundle_id.is_some_and(|id| DENIED_APPS.contains(&id)) {
        return Outbound::Skip(Reason::DeniedApp);
    }
    let context = current_sentence(s.preceding);
    match find_secret(context) {
        Some(name) => Outbound::WithoutContext(Reason::SecretPattern(name)),
        None => Outbound::WithContext(context.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decide_text(preceding: &str) -> Outbound {
        decide(&Snapshot { secure_input: false, bundle_id: Some("com.apple.TextEdit"), preceding })
    }

    fn secret(name: &'static str) -> Outbound {
        Outbound::WithoutContext(Reason::SecretPattern(name))
    }

    #[test]
    fn secure_input_skips_even_for_allowed_app() {
        let s = Snapshot { secure_input: true, bundle_id: Some("com.apple.TextEdit"), preceding: "こんにちは" };
        assert_eq!(decide(&s), Outbound::Skip(Reason::SecureInput));
    }

    #[test]
    fn secure_input_takes_priority_over_denied_app() {
        let s = Snapshot { secure_input: true, bundle_id: Some("com.apple.Terminal"), preceding: "" };
        assert_eq!(decide(&s), Outbound::Skip(Reason::SecureInput));
    }

    #[test]
    fn every_denied_app_is_skipped() {
        DENIED_APPS.iter().for_each(|id| {
            let s = Snapshot { secure_input: false, bundle_id: Some(id), preceding: "こんにちは" };
            assert_eq!(decide(&s), Outbound::Skip(Reason::DeniedApp), "bundle_id={id}");
        });
    }

    #[test]
    fn unknown_or_missing_bundle_id_is_allowed() {
        let missing = Snapshot { secure_input: false, bundle_id: None, preceding: "こんにちは" };
        assert_eq!(decide(&missing), Outbound::WithContext("こんにちは".to_string()));
        assert_eq!(decide_text("こんにちは"), Outbound::WithContext("こんにちは".to_string()));
    }

    #[test]
    fn each_secret_pattern_blocks_context() {
        let cases: &[(&str, &str)] = &[
            ("sk-abcdefgh12345678", "api_key_prefix"),
            ("ghp_abcdefghij", "api_key_prefix"),
            ("github_pat_abcdefghij", "api_key_prefix"),
            ("xoxb-12345678", "api_key_prefix"),
            ("AKIAIOSFODNN7EXAMPLE", "api_key_prefix"),
            ("AIzaSyA12345678", "api_key_prefix"),
            ("cfut_abcdefgh", "api_key_prefix"),
            ("glpat-abcdefgh", "api_key_prefix"),
            ("-----BEGIN RSA PRIVATE KEY-----", "private_key"),
            ("eyJhbGciOiJIUzI1.eyJzdWIiOiIxMjM0.abc", "jwt"),
            ("連絡先は taro.yamada+x@example.co.jp です", "email"),
            ("カード番号 4111 1111 1111 1111", "long_digits"),
            ("電話 090-1234-5678", "long_digits"),
            ("Zm9vYmFyYmF6cXV4MTIzNDU2Nzg5MA==", "high_entropy_token"),
            ("my Password is", "secret_word"),
            ("API_KEY を入れる", "secret_word"),
            ("api key", "secret_word"),
            ("パスワードを入力", "secret_word"),
            ("暗証番号は", "secret_word"),
            ("合言葉", "secret_word"),
            ("秘密鍵を", "secret_word"),
            ("認証コードが届いた", "secret_word"),
            ("ワンタイムの", "secret_word"),
        ];
        cases.iter().for_each(|(text, name)| {
            assert_eq!(decide_text(text), secret(name), "text={text}");
        });
    }

    #[test]
    fn ordinary_sentences_pass_through_as_context() {
        let cases = [
            "今日は医者に行く",
            "メールを送る",
            "2026年10月18日に会う",
            "3人で12345円",
            "abcdefghijklmnopqrstuvwxyz",
            "This is a normal English sentence",
            "",
        ];
        cases.iter().for_each(|text| {
            assert_eq!(decide_text(text), Outbound::WithContext(text.to_string()), "text={text}");
        });
    }

    #[test]
    fn letters_only_long_run_is_not_high_entropy() {
        assert!(matches!(decide_text("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), Outbound::WithContext(_)));
    }

    #[test]
    fn digits_only_long_run_is_caught_as_long_digits_not_entropy() {
        assert_eq!(decide_text("123456789012345678901234567"), secret("long_digits"));
    }

    #[test]
    fn context_is_cut_at_last_sentence_end() {
        assert_eq!(decide_text("昨日は雨だった。今日は晴れ"), Outbound::WithContext("今日は晴れ".to_string()));
        assert_eq!(decide_text("本当？はい!行く\nそして"), Outbound::WithContext("そして".to_string()));
    }

    #[test]
    fn secret_in_previous_sentence_does_not_block_current_one() {
        assert_eq!(
            decide_text("パスワードは秘密。今日は晴れ"),
            Outbound::WithContext("今日は晴れ".to_string())
        );
    }

    #[test]
    fn context_ending_with_sentence_end_is_empty() {
        assert_eq!(decide_text("おわり。"), Outbound::WithContext(String::new()));
    }

    #[test]
    fn context_keeps_only_last_100_chars() {
        let long = "あ".repeat(150);
        assert_eq!(decide_text(&long), Outbound::WithContext("あ".repeat(100)));
    }

    #[test]
    fn context_is_trimmed() {
        assert_eq!(decide_text("  こんにちは \n"), Outbound::WithContext("".to_string()));
        assert_eq!(decide_text("  こんにちは "), Outbound::WithContext("こんにちは".to_string()));
    }

    #[test]
    fn secret_beyond_100_chars_is_not_in_context() {
        let text = format!("sk-abcdefgh12345678{}", "あ".repeat(100));
        assert_eq!(decide_text(&text), Outbound::WithContext("あ".repeat(100)));
    }

    #[test]
    fn reason_names_do_not_contain_input() {
        assert_eq!(Reason::SecureInput.name(), "secure_input");
        assert_eq!(Reason::DeniedApp.name(), "denied_app");
        assert_eq!(Reason::SecretPattern("jwt").name(), "jwt");
    }
}
