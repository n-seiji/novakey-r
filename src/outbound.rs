#![forbid(unsafe_code)]
//! 外部 (Cloudflare) へ送る前のプライバシーガード。純粋関数と const テーブルだけで構成する。
use std::sync::LazyLock;

use regex::Regex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    SecureInput,
    DeniedApp,
    UnknownApp,
    SecretPattern(&'static str),
}

impl Reason {
    /// ログ用の名前。入力内容は含まない。
    pub fn name(&self) -> &'static str {
        match self {
            Reason::SecureInput => "secure_input",
            Reason::DeniedApp => "denied_app",
            Reason::UnknownApp => "unknown_app",
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
    /// 組成中の読みと候補も外へ出るので、文脈と同じ検査にかける。
    pub reading: &'a str,
    pub candidates: &'a [String],
}

/// パスワードを扱うアプリ。外への送信も、学習も止める。
const PASSWORD_MANAGERS: &[&str] = &[
    "com.1password.1password",
    "com.agilebits.onepassword7",
    "com.bitwarden.desktop",
    "com.apple.keychainaccess",
    "com.apple.Passwords",
    "org.keepassxc.keepassxc",
];

/// 外へ送らないアプリ (パスワード管理 + ターミナル)。ターミナルは学習は止めない。
const DENIED_APPS: &[&str] = &[
    "com.1password.1password",
    "com.agilebits.onepassword7",
    "com.bitwarden.desktop",
    "com.apple.keychainaccess",
    "com.apple.Passwords",
    "org.keepassxc.keepassxc",
    "com.apple.Terminal",
    "com.googlecode.iterm2",
    "dev.warp.Warp-Stable",
    "com.mitchellh.ghostty",
    "net.kovidgoyal.kitty",
    "org.alacritty",
    "com.github.wez.wezterm",
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

fn has_ten_digits(s: &str) -> bool {
    s.chars().filter(char::is_ascii_digit).count() >= 10
}

macro_rules! key_prefixes {
    () => {
        "sk-|sk_live_|pk_live_|ghp_|gho_|ghs_|github_pat_|xox[abposr]-|AKIA|ASIA|AIza|cfut_|glpat-"
    };
}

/// 文脈 (preceding)、読み、候補のどれにもかける。
const SECRET_PATTERNS: &[Pattern] = &[
    Pattern {
        name: "api_key_prefix",
        regex: concat!("(", key_prefixes!(), r")[A-Za-z0-9_\-]{8,}"),
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
    Pattern { name: "long_digits", regex: r"\d[\d \-]{8,}\d", accept: has_ten_digits },
    Pattern {
        name: "high_entropy_token",
        regex: r"[A-Za-z0-9_\-+/=]{24,}",
        accept: has_letter_and_digit,
    },
];

/// 直前の文字列だけにかける。読みや候補に「パスワード」と打つのは普通なので、そちらには使わない。
const CONTEXT_ONLY_PATTERNS: &[Pattern] = &[Pattern {
    name: "secret_word",
    regex: r"(?i)(password|passwd|passcode|secret|token|api[ _-]?key)|パスワード|暗証番号|合言葉|秘密鍵|認証コード|ワンタイム",
    accept: any_match,
}];

/// 読みと候補だけにかける。確定前の入力途中 (打ちかけの鍵や番号) を送らないための早めの検査。
const IN_PROGRESS_PATTERNS: &[Pattern] = &[
    Pattern { name: "partial_digits", regex: r"\d(?:[ \-]?\d){5,}", accept: any_match },
    Pattern {
        name: "partial_key_prefix",
        regex: concat!(r"(?:^|[^A-Za-z0-9])(?:", key_prefixes!(), ")"),
        accept: any_match,
    },
    Pattern { name: "long_token", regex: r"[A-Za-z0-9_\-]{12,}", accept: any_match },
];

type Compiled = Vec<(&'static Pattern, Regex)>;

fn compile(patterns: &[&'static [Pattern]]) -> Compiled {
    patterns
        .iter()
        .flat_map(|table| table.iter())
        .map(|p| (p, Regex::new(p.regex).expect("秘密パターンの正規表現は有効")))
        .collect()
}

static CONTEXT_PATTERNS: LazyLock<Compiled> =
    LazyLock::new(|| compile(&[SECRET_PATTERNS, CONTEXT_ONLY_PATTERNS]));
static PAYLOAD_PATTERNS: LazyLock<Compiled> =
    LazyLock::new(|| compile(&[SECRET_PATTERNS, IN_PROGRESS_PATTERNS]));

/// 直近の文 (最後の文末記号より後ろ) の末尾 CONTEXT_MAX_CHARS 文字を trim して返す。
fn current_sentence(preceding: &str) -> &str {
    let tail = preceding.rsplit(SENTENCE_ENDS).next().unwrap_or("");
    let skip = tail.chars().count().saturating_sub(CONTEXT_MAX_CHARS);
    tail.char_indices()
        .nth(skip)
        .map_or("", |(start, _)| &tail[start..])
        .trim()
}

fn find_secret(patterns: &Compiled, text: &str) -> Option<&'static str> {
    patterns
        .iter()
        .find(|(p, re)| re.find_iter(text).any(|m| (p.accept)(m.as_str())))
        .map(|(p, _)| p.name)
}

/// 確定を学習してよいか。セキュア入力中・パスワード管理アプリでは学習しない (ファイルに入力が残るため)。
pub fn learning_allowed(secure_input: bool, bundle_id: Option<&str>) -> bool {
    !secure_input && !bundle_id.is_some_and(|id| PASSWORD_MANAGERS.contains(&id))
}

pub fn decide(s: &Snapshot) -> Outbound {
    if s.secure_input {
        return Outbound::Skip(Reason::SecureInput);
    }
    // 送信元アプリが分からないときは、拒否リストに当たるか確認できないので送らない。
    let Some(bundle_id) = s.bundle_id else {
        return Outbound::Skip(Reason::UnknownApp);
    };
    if DENIED_APPS.contains(&bundle_id) {
        return Outbound::Skip(Reason::DeniedApp);
    }
    let payload = &*PAYLOAD_PATTERNS;
    if let Some(name) = find_secret(payload, s.reading).or_else(|| s.candidates.iter().find_map(|c| find_secret(payload, c))) {
        return Outbound::Skip(Reason::SecretPattern(name));
    }
    // 文区切りや切り詰めの前に窓全体を検査する (境界をまたぐ鍵や改行をまたぐ「パスワード\nxxx」を逃さない)。
    if let Some(name) = find_secret(&CONTEXT_PATTERNS, s.preceding) {
        return Outbound::WithoutContext(Reason::SecretPattern(name));
    }
    Outbound::WithContext(current_sentence(s.preceding).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decide_text(preceding: &str) -> Outbound {
        decide(&Snapshot { secure_input: false, bundle_id: Some("com.apple.TextEdit"), preceding, reading: "", candidates: &[] })
    }

    fn secret(name: &'static str) -> Outbound {
        Outbound::WithoutContext(Reason::SecretPattern(name))
    }

    #[test]
    fn secure_input_skips_even_for_allowed_app() {
        let s = Snapshot { secure_input: true, bundle_id: Some("com.apple.TextEdit"), preceding: "こんにちは", reading: "", candidates: &[] };
        assert_eq!(decide(&s), Outbound::Skip(Reason::SecureInput));
    }

    #[test]
    fn secure_input_takes_priority_over_denied_app() {
        let s = Snapshot { secure_input: true, bundle_id: Some("com.apple.Terminal"), preceding: "", reading: "", candidates: &[] };
        assert_eq!(decide(&s), Outbound::Skip(Reason::SecureInput));
    }

    #[test]
    fn every_denied_app_is_skipped() {
        DENIED_APPS.iter().for_each(|id| {
            let s = Snapshot { secure_input: false, bundle_id: Some(id), preceding: "こんにちは", reading: "", candidates: &[] };
            assert_eq!(decide(&s), Outbound::Skip(Reason::DeniedApp), "bundle_id={id}");
        });
    }

    #[test]
    fn password_managers_are_a_subset_of_denied_apps() {
        assert!(PASSWORD_MANAGERS.iter().all(|id| DENIED_APPS.contains(id)));
    }

    #[test]
    fn learning_is_blocked_for_secure_input_and_password_managers_only() {
        assert!(learning_allowed(false, Some("com.apple.TextEdit")));
        assert!(learning_allowed(false, None));
        assert!(!learning_allowed(true, Some("com.apple.TextEdit")));
        assert!(!learning_allowed(true, None));
        for id in PASSWORD_MANAGERS {
            assert!(!learning_allowed(false, Some(id)), "{id}");
        }
        assert!(learning_allowed(false, Some("com.apple.Terminal")), "ターミナルでは学習する");
    }

    #[test]
    fn unknown_bundle_id_is_allowed_but_missing_one_is_skipped() {
        let missing = Snapshot { secure_input: false, bundle_id: None, preceding: "こんにちは", reading: "", candidates: &[] };
        assert_eq!(decide(&missing), Outbound::Skip(Reason::UnknownApp));
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
            ("09012345678", "long_digits"),
            ("0312345678", "long_digits"),
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
            "2026年10月18日 12時30分",
            "123456789円",
            "2026-10-23",
            "2020 - 2026年",
            "1000 - 2000円",
            "1 2 3 4 5 6",
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
    fn secret_in_previous_sentence_blocks_context() {
        assert_eq!(decide_text("パスワードは秘密。今日は晴れ"), secret("secret_word"));
        assert_eq!(decide_text("パスワード\nAbc12345"), secret("secret_word"));
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
    fn secret_beyond_100_chars_blocks_context() {
        let text = format!("sk-abcdefgh12345678{}", "あ".repeat(100));
        assert_eq!(decide_text(&text), secret("api_key_prefix"));
    }

    #[test]
    fn secret_straddling_the_100_char_cut_blocks_context() {
        let text = format!("sk-abcdefgh12345678{}", "あ".repeat(85));
        assert_eq!(decide_text(&text), secret("api_key_prefix"));
    }

    fn decide_payload(reading: &str, candidates: &[String]) -> Outbound {
        decide(&Snapshot {
            secure_input: false,
            bundle_id: Some("com.apple.TextEdit"),
            preceding: "こんにちは",
            reading,
            candidates,
        })
    }

    #[test]
    fn secret_in_reading_skips_request() {
        let skip = |name| Outbound::Skip(Reason::SecretPattern(name));
        assert_eq!(decide_payload("090123456789", &[]), skip("long_digits"));
        assert_eq!(decide_payload("あんしょうばんごう 1234 5678 9012", &[]), skip("long_digits"));
    }

    #[test]
    fn typing_a_secret_word_as_reading_or_candidate_is_not_skipped() {
        let ctx = Outbound::WithContext("こんにちは".to_string());
        let candidates = vec!["パスワード".to_string(), "ぱすわーど".to_string()];
        assert_eq!(decide_payload("ぱすわーど", &candidates), ctx);
        assert_eq!(decide_payload("ひみつかぎ", &["秘密鍵".to_string()]), ctx);
        assert_eq!(decide_payload("あいことば", &["合言葉".to_string()]), ctx);
        assert_eq!(decide_payload("わんたいむ", &["ワンタイム".to_string()]), ctx);
    }

    #[test]
    fn secret_being_typed_skips_request() {
        let skip = |name| Outbound::Skip(Reason::SecretPattern(name));
        assert_eq!(decide_payload("0901234", &[]), skip("partial_digits"));
        assert_eq!(decide_payload("090-123", &[]), skip("partial_digits"));
        assert_eq!(decide_payload("あ", &["sk-abc".to_string()]), skip("partial_key_prefix"));
        assert_eq!(decide_payload("あ", &["ghp_".to_string()]), skip("partial_key_prefix"));
        assert_eq!(decide_payload("あ", &["abcdefghijkl".to_string()]), skip("long_token"));
    }

    #[test]
    fn ordinary_typing_is_not_skipped() {
        let ctx = Outbound::WithContext("こんにちは".to_string());
        assert_eq!(decide_payload("2026ねん", &["2026年".to_string()]), ctx);
        assert_eq!(decide_payload("ひゃくえん", &["100円".to_string()]), ctx);
        assert_eq!(decide_payload("ですくとっぷ", &["desktop".to_string(), "task-force".to_string()]), ctx);
    }

    #[test]
    fn secret_in_any_candidate_skips_request() {
        let candidates = vec!["今日".to_string(), "sk-abcdefgh12345678".to_string()];
        assert_eq!(decide_payload("きょう", &candidates), Outbound::Skip(Reason::SecretPattern("api_key_prefix")));
        let candidates = vec!["今日".to_string(), "京".to_string()];
        assert_eq!(decide_payload("きょう", &candidates), Outbound::WithContext("こんにちは".to_string()));
    }

    #[test]
    fn reason_names_do_not_contain_input() {
        assert_eq!(Reason::SecureInput.name(), "secure_input");
        assert_eq!(Reason::DeniedApp.name(), "denied_app");
        assert_eq!(Reason::UnknownApp.name(), "unknown_app");
        assert_eq!(Reason::SecretPattern("jwt").name(), "jwt");
    }
}
