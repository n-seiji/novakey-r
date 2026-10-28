#![forbid(unsafe_code)]
//! アプリごとの「Shizuku を通して直前に打った文字列」。変換の文脈と記号の幅の判断に使う。
//! アプリ側の文字は読まない。メモリ上だけに置き、ファイルにもログにも書かない。時刻は呼び出し側から渡すので、純粋に試せる。

use std::cell::RefCell;
use std::time::{Duration, Instant};

/// 覚えるアプリの数。使った順に新しい 10 個。
const MAX_APPS: usize = 10;
/// 1 アプリで覚える文字数 (後ろから残す)。文字で持つのはこの量で、審判が使うのは後ろの `judge::CONTEXT_TOKENS` (32 トークン) だけ。
const MAX_CHARS: usize = 100;
/// 最後の入力からこれだけ経つと、別の作業と見て文脈を捨てる。`Instant` なので、スリープの間は数えない (起動中の 12 時間)。
const STALE_AFTER: Duration = Duration::from_secs(12 * 60 * 60);

/// 英語のアプリかを決める投票の窓 (直近の確定の数)。
const WINDOW: u32 = 32;
/// 窓にこれだけ溜まるまでは英語のアプリと見ない。
const MIN_VOTES: u32 = 12;

struct Entry {
    app: String,
    text: String,
    last: Instant,
    /// 直近の投票。最新が bit 0 で、1 が英語。数えるのは下位 `len` ビットだけ。文字は持たない。
    votes: u32,
    /// 溜まった投票の数 (`WINDOW` まで)。
    len: u8,
}

/// 仮名・漢字・カタカナ (半角を含む) か。
fn is_japanese(c: char) -> bool {
    matches!(c, '\u{3040}'..='\u{30FF}' | '\u{3400}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}' | '\u{FF66}'..='\u{FF9F}')
}

/// 1 回の入力が英語の票 (Some(true))・日本語の票 (Some(false))・票なし (None) のどれか。
/// 仮名・漢字を 1 つでも含めば日本語 (`lineに` のような混在も)、なければ ASCII の英字を含めば英語。数字・記号・空白・改行だけなら票なし。
fn vote_of(text: &str) -> Option<bool> {
    if text.chars().any(is_japanese) {
        Some(false)
    } else if text.chars().any(|c| c.is_ascii_alphabetic()) {
        Some(true)
    } else {
        None
    }
}

impl Entry {
    fn push_vote(&mut self, english: bool) {
        self.votes = (self.votes << 1) | u32::from(english);
        self.len = (u32::from(self.len) + 1).min(WINDOW) as u8;
    }

    fn is_english(&self) -> bool {
        let len = u32::from(self.len);
        let mask = if len >= WINDOW { u32::MAX } else { (1u32 << len) - 1 };
        len >= MIN_VOTES && (self.votes & mask).count_ones() * 4 >= len * 3
    }
}

/// アプリの bundle id ごとの文脈。先頭が最も古く、末尾が最近使ったもの。
#[derive(Default)]
pub struct ContextStore {
    entries: Vec<Entry>,
}

impl ContextStore {
    /// `text` を `app` の文脈の末尾に足す。改行は文の切れ目として残す (素通しの Backspace と数が合うよう、続く改行も全部残す)。
    pub fn record(&mut self, app: &str, text: &str, now: Instant) {
        if text.is_empty() {
            return;
        }
        let mut entry = match self.entries.iter().position(|e| e.app == app) {
            Some(i) => self.entries.remove(i),
            None => Entry { app: app.to_string(), text: String::new(), last: now, votes: 0, len: 0 },
        };
        if now.saturating_duration_since(entry.last) >= STALE_AFTER {
            entry.text.clear();
            entry.votes = 0;
            entry.len = 0;
        }
        if let Some(english) = vote_of(text) {
            entry.push_vote(english);
        }
        entry.text.push_str(text);
        let extra = entry.text.chars().count().saturating_sub(MAX_CHARS);
        if extra > 0 {
            let cut = entry.text.char_indices().nth(extra).map_or(entry.text.len(), |(i, _)| i);
            entry.text.drain(..cut);
        }
        entry.last = now;
        if self.entries.len() == MAX_APPS {
            self.entries.remove(0);
        }
        self.entries.push(entry);
    }

    /// `app` の文脈。12 時間 (起動中) 以上入力が無ければ空。
    pub fn get(&self, app: &str, now: Instant) -> &str {
        self.entries
            .iter()
            .find(|e| e.app == app && now.saturating_duration_since(e.last) < STALE_AFTER)
            .map_or("", |e| e.text.as_str())
    }

    /// `app` が英語を打つアプリか (直近 32 回の確定の 75% 以上が英語で、12 回以上溜まっているとき)。知らない・12 時間たったアプリは false。
    pub fn is_english_app(&self, app: &str, now: Instant) -> bool {
        self.entries.iter().find(|e| e.app == app && now.saturating_duration_since(e.last) < STALE_AFTER).is_some_and(Entry::is_english)
    }

    /// 後ろの 1 文字を取り除く (素通しの Backspace)。
    pub fn pop(&mut self, app: &str) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.app == app) {
            e.text.pop();
        }
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
}

/// 共有の文脈を使う。
pub fn with<T>(f: impl FnOnce(&mut ContextStore) -> T) -> T {
    STORE.with(|s| f(&mut s.borrow_mut()))
}

/// 文脈の最後の行 (最後の改行より後ろ)。新しい行の頭では空になる。
fn current_line(context: &str) -> &str {
    context.rsplit('\n').next().unwrap_or("")
}

/// 今の行の末尾 (空白を除く) が ASCII の英数字か記号か。行が空のときは false (前の行が英語でも見ない)。
/// true なら、半角で書いている最中と見て `! ? ( ) { } . , : ;` を半角のまま入れる。
pub fn ends_ascii(context: &str) -> bool {
    current_line(context).trim_end().chars().next_back().is_some_and(|c| c.is_ascii() && !c.is_ascii_control())
}

/// 文脈の今の行が空白だけか (行の頭、またはインデントだけ)。
pub fn at_line_start(context: &str) -> bool {
    current_line(context).trim().is_empty()
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
    fn 改行は切れ目として全部残り_バックスペースと数が合う() {
        let (mut s, t) = (ContextStore::default(), Instant::now());
        s.record("a", "前の行\n次の", t);
        assert_eq!(s.get("a", t), "前の行\n次の");
        s.record("a", "\n\n", t);
        assert_eq!(s.get("a", t), "前の行\n次の\n\n");
        s.pop("a");
        assert_eq!(s.get("a", t), "前の行\n次の\n");
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
    fn 数時間たっても文脈は残り_十二時間たつと使わず_次の確定は新しく始まる() {
        let (mut s, t) = (ContextStore::default(), Instant::now());
        s.record("a", "古い", t);
        let later = t + STALE_AFTER;
        assert_eq!(s.get("a", t + Duration::from_secs(6 * 60 * 60)), "古い");
        assert_eq!(s.get("a", t + STALE_AFTER - Duration::from_secs(1)), "古い");
        assert_eq!(s.get("a", later), "");
        s.record("a", "新しい", later);
        assert_eq!(s.get("a", later), "新しい");
    }

    #[test]
    fn 後ろの一文字を消し_全部を捨てられる() {
        let (mut s, t) = (ContextStore::default(), Instant::now());
        s.record("a", "ab", t);
        s.record("b", "cd", t);
        s.pop("a");
        assert_eq!(s.get("a", t), "a");
        assert_eq!(s.get("b", t), "cd");
        s.clear_all();
        assert_eq!(s.get("b", t), "");
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
    }

    #[test]
    fn 新しい行の頭では前の行の末尾を見ない() {
        assert!(!ends_ascii("hello\n"));
        assert!(!ends_ascii("hello\n今日は"));
        assert!(ends_ascii("今日は\nhello"));
        assert!(ends_ascii("今日は\nhello \n\nworld"));
    }

    #[test]
    fn 行の頭かインデントだけのときだけ行頭と見る() {
        for c in ["", "x\n", "x\n  ", "\n\t"] {
            assert!(at_line_start(c), "{c:?}");
        }
        for c in ["これは", "hello", "x\n  y"] {
            assert!(!at_line_start(c), "{c:?}");
        }
    }

    fn votes(s: &mut ContextStore, t: Instant, english: usize, japanese: usize) {
        for _ in 0..english {
            s.record("a", "word ", t);
        }
        for _ in 0..japanese {
            s.record("a", "ことば", t);
        }
    }

    #[test]
    fn 票の分類は仮名漢字があれば日本語_なければ英字で英語_それ以外は票なし() {
        for (text, want) in [
            ("fix ", Some(true)),
            ("PR", Some(true)),
            ("v1.2", Some(true)),
            ("あ", Some(false)),
            ("漢字", Some(false)),
            ("カタカナ", Some(false)),
            ("ｶﾀｶﾅ", Some(false)),
            ("lineに", Some(false)),
            ("artifactを", Some(false)),
            ("123", None),
            ("@", None),
            (" ", None),
            ("\n", None),
            ("ＡＢＣ", None),
            ("😀", None),
        ] {
            assert_eq!(vote_of(text), want, "{text:?}");
        }
    }

    #[test]
    fn 窓の中の英語が七割五分以上で十二票以上のとき英語のアプリ() {
        let t = Instant::now();
        let mut s = ContextStore::default();
        votes(&mut s, t, 11, 0);
        assert!(!s.is_english_app("a", t), "11 票は信用しない");
        votes(&mut s, t, 1, 0);
        assert!(s.is_english_app("a", t));
        let mut s = ContextStore::default();
        votes(&mut s, t, 24, 8);
        assert!(s.is_english_app("a", t));
        let mut s = ContextStore::default();
        votes(&mut s, t, 23, 9);
        assert!(!s.is_english_app("a", t));
        let mut s = ContextStore::default();
        votes(&mut s, t, 32, 32);
        assert!(!s.is_english_app("a", t), "古い英語は窓から出る");
        assert!(!s.is_english_app("b", t), "知らないアプリ");
    }

    #[test]
    fn 票なしの入力は数えず_十二時間で票も消え_追い出しと全消去でも消える() {
        let t = Instant::now();
        let mut s = ContextStore::default();
        votes(&mut s, t, 12, 0);
        s.record("a", "123 \n", t);
        assert!(s.is_english_app("a", t));
        assert_eq!(s.entries[0].len, 12);
        s.pop("a");
        assert!(s.is_english_app("a", t), "pop は票に触れない");
        assert!(!s.is_english_app("a", t + STALE_AFTER), "12 時間たつと使わない");
        s.record("a", "word", t + STALE_AFTER);
        assert_eq!(s.entries[0].len, 1, "古い票は捨てて数え直す");
        votes(&mut s, t, 12, 0);
        for i in 0..10 {
            s.record(&format!("app{i}"), "x", t);
        }
        assert!(!s.is_english_app("a", t), "追い出されると票も消える");
        votes(&mut s, t, 12, 0);
        s.clear_all();
        assert!(!s.is_english_app("a", t));
    }
}
