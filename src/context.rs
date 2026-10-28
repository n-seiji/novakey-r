#![forbid(unsafe_code)]
//! アプリごとの「直前に確定した文字列」。変換の文脈と記号の幅の判断に使う。
//! メモリ上だけに置き、ファイルにもログにも書かない。時刻は呼び出し側から渡すので、純粋に試せる。

use std::cell::RefCell;
use std::collections::HashSet;
use std::time::{Duration, Instant};

/// 覚えるアプリの数。使った順に新しい 10 個。
const MAX_APPS: usize = 10;
/// 1 アプリで覚える文字数 (後ろから残す)。文字で持つのはこの量で、審判が使うのは後ろの `judge::CONTEXT_TOKENS` (32 トークン) だけ。
const MAX_CHARS: usize = 100;
/// 最後の確定からこれだけ経つと、カーソルが動いたと見て文脈を捨てる。
const STALE_AFTER: Duration = Duration::from_secs(10 * 60);

struct Entry {
    app: String,
    text: String,
    last: Instant,
}

/// `text` を `context` の末尾に足す。改行より前を捨て、後ろから `MAX_CHARS` 文字だけ残す。
pub fn append_capped(context: &mut String, text: &str) {
    context.push_str(text);
    if let Some(i) = context.rfind('\n') {
        context.drain(..=i);
    }
    let extra = context.chars().count().saturating_sub(MAX_CHARS);
    if extra > 0 {
        let cut = context.char_indices().nth(extra).map_or(context.len(), |(i, _)| i);
        context.drain(..cut);
    }
}

/// ホストアプリのカーソル前を読む範囲 (UTF-16 単位)。審判が使うのは後ろの 32 トークンだけ。
pub const MAX_UTF16: usize = 100;
/// 1 回の読み取りがこれを超えたら遅いと数える。
pub const SLOW_READ: Duration = Duration::from_millis(15);
/// 遅い読み取りがこれだけ続いたら、そのアプリでは読み取りをやめる (速い読み取りで数え直す)。
pub const SLOW_READS_TO_DISABLE: u8 = 3;
/// 覚えている文脈がこの時間内に足されたものなら、ホストがまだ反映していないかもしれないと見る。
pub const STORE_FRESH: Duration = Duration::from_secs(2);

/// ホストアプリのカーソル前の文字列を読んだ結果。
#[derive(Debug, PartialEq, Eq)]
pub enum HostRead {
    /// 読めた (カーソルの行頭から。空もありうる)。
    Text(String),
    /// このアプリは答えない (nil・位置が不明・先頭 0)。覚えている文脈を使う。
    Unsupported,
    /// 呼び出しが例外で失敗した。そのアプリでは読み取りをやめる。
    Failed,
    /// 要求した範囲より長い答え (範囲を無視するホスト)。カーソルの後ろの文字が混ざるので使わず、そのアプリでは読み取りをやめる。
    Garbage,
}

/// カーソル位置 `location` (UTF-16 単位、不明なら None) の前の最大 `MAX_UTF16` 単位を `read(start, len)` で読み、整える。
/// `read` は呼び出しの失敗を `None`、nil を `Some(None)` で返す。
/// 位置 0 は信用しない: 常に {0,0} を返すホスト (一部の Electron・端末) がある。Unsupported にして覚えている文脈に任せる。
pub fn before_caret(location: Option<usize>, read: impl FnOnce(usize, usize) -> Option<Option<String>>) -> HostRead {
    let Some(location) = location.filter(|&l| l > 0) else { return HostRead::Unsupported };
    let start = location.saturating_sub(MAX_UTF16);
    let len = location - start;
    match read(start, len) {
        None => HostRead::Failed,
        Some(None) => HostRead::Unsupported,
        Some(Some(text)) if text.encode_utf16().count() > len => HostRead::Garbage,
        Some(Some(text)) => HostRead::Text(tidy(&text)),
    }
}

/// 割れたサロゲートペア (文書にある本物の U+FFFD も) と、最後の改行より前を落とす。
fn tidy(text: &str) -> String {
    let tail = text.to_string();
    let tail = tail.strip_prefix('\u{FFFD}').unwrap_or(&tail);
    let cut = tail.char_indices().rev().find(|(_, c)| matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')).map_or(0, |(i, c)| i + c.len_utf8());
    tail[cut..].to_string()
}

/// アプリの bundle id ごとの文脈。先頭が最も古く、末尾が最近使ったもの。
#[derive(Default)]
pub struct ContextStore {
    entries: Vec<Entry>,
}

impl ContextStore {
    /// `text` を `app` の文脈の末尾に足す。改行より前は捨てる (改行は新しい文の始まりと見る)。
    pub fn record(&mut self, app: &str, text: &str, now: Instant) {
        if text.is_empty() {
            return;
        }
        let mut entry = match self.entries.iter().position(|e| e.app == app) {
            Some(i) => self.entries.remove(i),
            None => Entry { app: app.to_string(), text: String::new(), last: now },
        };
        if now.saturating_duration_since(entry.last) >= STALE_AFTER {
            entry.text.clear();
        }
        append_capped(&mut entry.text, text);
        entry.last = now;
        if self.entries.len() == MAX_APPS {
            self.entries.remove(0);
        }
        self.entries.push(entry);
    }

    /// `app` の文脈のうち、`within` 以内に足されたもの。無ければ空。
    pub fn fresh(&self, app: &str, now: Instant, within: Duration) -> &str {
        self.entries.iter().find(|e| e.app == app && now.saturating_duration_since(e.last) < within).map_or("", |e| e.text.as_str())
    }

    /// `app` の文脈。10 分以上確定が無ければ空。
    pub fn get(&self, app: &str, now: Instant) -> &str {
        self.entries
            .iter()
            .find(|e| e.app == app && now.saturating_duration_since(e.last) < STALE_AFTER)
            .map_or("", |e| e.text.as_str())
    }

    /// 後ろの 1 文字を取り除く (素通しの Backspace)。
    pub fn pop(&mut self, app: &str) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.app == app) {
            e.text.pop();
        }
    }

    pub fn clear(&mut self, app: &str) {
        self.entries.retain(|e| e.app != app);
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn clear_all(&mut self) {
        self.entries.clear();
    }
}

thread_local! {
    /// プロセス全体の文脈。コントローラはすべてメインスレッドで動くので、ロックは要らない。
    static STORE: RefCell<ContextStore> = RefCell::new(ContextStore::default());
    static SLOW_COUNT: RefCell<std::collections::HashMap<String, u8>> = RefCell::new(std::collections::HashMap::new());
    static DISABLED: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

/// 読み取りをやめたアプリ (ホストが遅い・例外を投げる)。プロセスが続く間そのまま。
pub fn disable_host_read(app: &str) {
    DISABLED.with(|d| d.borrow_mut().insert(app.to_string()));
}

/// 読み取りの速さを数える。遅い読み取りが `SLOW_READS_TO_DISABLE` 回続いたら true (やめるべき)。
pub fn note_read_speed(app: &str, slow: bool) -> bool {
    SLOW_COUNT.with(|c| {
        let mut c = c.borrow_mut();
        let n = c.entry(app.to_string()).or_insert(0);
        *n = if slow { n.saturating_add(1) } else { 0 };
        *n >= SLOW_READS_TO_DISABLE
    })
}

pub fn host_read_disabled(app: &str) -> bool {
    DISABLED.with(|d| d.borrow().contains(app))
}

/// 共有の文脈を使う。
pub fn with<T>(f: impl FnOnce(&mut ContextStore) -> T) -> T {
    STORE.with(|s| f(&mut s.borrow_mut()))
}

/// 文脈の末尾 (空白を除く) が ASCII で、その最後の語に ASCII の英字があるか。文脈が無いときは false。
/// true なら、半角で書いている最中と見て `! ? ( ) { } . , : ;` を半角のまま入れる。
/// 行頭の `- ` `1. ` `> ` `## ` `// ` や日付のように英字の無い記号・数字だけの語は、英語を書いている印にしない。
pub fn ends_ascii(context: &str) -> bool {
    let context = context.trim_end();
    let word = context.rsplit(char::is_whitespace).next().unwrap_or("");
    context.chars().next_back().is_some_and(|c| c.is_ascii() && !c.is_ascii_control()) && word.chars().any(|c| c.is_ascii_alphabetic())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 追記した文字列を返し_知らないアプリは空() {
        let (mut s, t) = (ContextStore::default(), Instant::now());
        s.record("a", "机の", t);
        s.record("a", "上", t);
        assert_eq!(s.get("a", t), "机の上");
        assert_eq!(s.get("b", t), "");
    }

    #[test]
    fn 百文字を超えたら後ろから残す() {
        let (mut s, t) = (ContextStore::default(), Instant::now());
        s.record("a", &"あ".repeat(90), t);
        s.record("a", &"い".repeat(30), t);
        let got = s.get("a", t);
        assert_eq!(got.chars().count(), MAX_CHARS);
        assert!(got.starts_with('あ') && got.ends_with('い'));
        assert_eq!(got.chars().filter(|&c| c == 'い').count(), 30);
    }

    #[test]
    fn 改行より前は捨てる() {
        let (mut s, t) = (ContextStore::default(), Instant::now());
        s.record("a", "前の行\n次の", t);
        assert_eq!(s.get("a", t), "次の");
        s.record("a", "\n", t);
        assert_eq!(s.get("a", t), "");
    }

    #[test]
    fn 十個を超えると最も使っていないアプリを忘れる() {
        let (mut s, t) = (ContextStore::default(), Instant::now());
        for i in 0..10 {
            s.record(&format!("app{i}"), "x", t);
        }
        s.record("app0", "y", t);
        s.record("app10", "z", t);
        assert_eq!(s.get("app0", t), "xy", "最近使ったアプリは残る");
        assert_eq!(s.get("app1", t), "", "最も使っていないアプリが追い出される");
        assert_eq!(s.get("app10", t), "z");
    }

    #[test]
    fn 十分たつと古い文脈は使わず_次の確定は新しく始まる() {
        let (mut s, t) = (ContextStore::default(), Instant::now());
        s.record("a", "古い", t);
        let later = t + STALE_AFTER;
        assert_eq!(s.get("a", t + STALE_AFTER - Duration::from_secs(1)), "古い");
        assert_eq!(s.get("a", later), "");
        s.record("a", "新しい", later);
        assert_eq!(s.get("a", later), "新しい");
    }

    #[test]
    fn 後ろの一文字を消し_アプリごとに捨てられる() {
        let (mut s, t) = (ContextStore::default(), Instant::now());
        s.record("a", "ab", t);
        s.record("b", "cd", t);
        s.pop("a");
        assert_eq!(s.get("a", t), "a");
        s.clear("a");
        assert_eq!(s.get("a", t), "");
        assert_eq!(s.get("b", t), "cd");
        s.clear_all();
        assert_eq!(s.get("b", t), "");
    }

    fn host(text: &str, caret: usize) -> HostRead {
        let units: Vec<u16> = text.encode_utf16().collect();
        before_caret(Some(caret), |start, len| {
            let end = (start + len).min(units.len());
            Some(Some(String::from_utf16_lossy(&units[start.min(end)..end])))
        })
    }

    #[test]
    fn ホストの文脈は改行より後ろだけ() {
        // "今日は\n明日も晴れ": 「今日は」3 + 改行 1 + 「明日も晴れ」5
        assert_eq!(host("今日は\n明日も晴れ", 9), HostRead::Text("明日も晴れ".into()));
        assert_eq!(host("今日は\n", 4), HostRead::Text(String::new()));
        assert_eq!(host("a\r\nb", 4), HostRead::Text("b".into()));
    }

    #[test]
    fn ホストの文脈は後ろの百単位だけ() {
        let text = format!("{}{}", "あ".repeat(50), "い".repeat(100));
        assert_eq!(host(&text, 150), HostRead::Text("い".repeat(100)));
    }

    #[test]
    fn 切れ目がサロゲートペアの途中でも壊れた文字を残さない() {
        let text = format!("😀{}", "a".repeat(99)); // 101 単位。先頭 1 単位を落とすと絵文字が割れる
        assert_eq!(host(&text, 101), HostRead::Text("a".repeat(99)));
    }

    #[test]
    fn 範囲を無視して全文を返すホストは使わず_失敗扱い() {
        let doc = "x".repeat(300);
        let got = before_caret(Some(250), |_, len| {
            assert_eq!(len, MAX_UTF16);
            Some(Some(doc.clone()))
        });
        assert_eq!(got, HostRead::Garbage);
    }

    #[test]
    fn 遅い読み取りが三回続いたときだけやめ_速い読み取りで数え直す() {
        assert!(!note_read_speed("slow.count", true));
        assert!(!note_read_speed("slow.count", true));
        assert!(!note_read_speed("slow.count", false));
        assert!(!note_read_speed("slow.count", true));
        assert!(!note_read_speed("slow.count", true));
        assert!(note_read_speed("slow.count", true));
    }

    #[test]
    fn 新しい文脈だけを返す() {
        let (mut s, t) = (ContextStore::default(), Instant::now());
        s.record("a", "x", t);
        assert_eq!(s.fresh("a", t + Duration::from_secs(1), STORE_FRESH), "x");
        assert_eq!(s.fresh("a", t + STORE_FRESH, STORE_FRESH), "");
    }

    #[test]
    fn 位置が不明か_0_か_nil_は答えなしで_例外は失敗() {
        assert_eq!(before_caret(None, |_, _| panic!("読まない")), HostRead::Unsupported);
        assert_eq!(before_caret(Some(0), |_, _| panic!("読まない")), HostRead::Unsupported);
        assert_eq!(before_caret(Some(5), |_, _| Some(None)), HostRead::Unsupported);
        assert_eq!(before_caret(Some(5), |_, _| None), HostRead::Failed);
    }

    #[test]
    fn 追記は改行で切り_百文字に収める() {
        let mut s = "前\n".to_string();
        append_capped(&mut s, "次");
        assert_eq!(s, "次");
        append_capped(&mut s, &"あ".repeat(120));
        assert_eq!(s.chars().count(), MAX_CHARS);
    }

    #[test]
    fn 末尾が_ascii_のときだけ半角と見る() {
        assert!(ends_ascii("hello"));
        assert!(ends_ascii("hello "), "末尾の空白は見ない");
        assert!(ends_ascii("a1"));
        assert!(ends_ascii("done."));
        assert!(!ends_ascii(""));
        assert!(!ends_ascii("   "));
        assert!(!ends_ascii("今日は"));
        assert!(!ends_ascii("hello。"));
        assert!(ends_ascii("claude "));
        for markup in ["- ", "* ", "1. ", "> ", "## ", "// ", "2026/10/28 ", "# "] {
            assert!(!ends_ascii(markup), "{markup:?}");
        }
        assert!(ends_ascii("- item"), "最後の語に英字があれば半角");
    }
}
