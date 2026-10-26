#![forbid(unsafe_code)]
//! アプリごとの「直前に確定した文字列」。変換の文脈と記号の幅の判断に使う。
//! メモリ上だけに置き、ファイルにもログにも書かない。時刻は呼び出し側から渡すので、純粋に試せる。

use std::cell::RefCell;
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
        entry.text.push_str(text);
        if let Some(i) = entry.text.rfind('\n') {
            entry.text.drain(..=i);
        }
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
}

/// 共有の文脈を使う。
pub fn with<T>(f: impl FnOnce(&mut ContextStore) -> T) -> T {
    STORE.with(|s| f(&mut s.borrow_mut()))
}

/// 文脈の末尾 (空白を除く) が ASCII の英数字か記号か。文脈が無いときは false。
/// true なら、半角で書いている最中と見て `! ? ( ) { } . , : ;` を半角のまま入れる。
pub fn ends_ascii(context: &str) -> bool {
    context.trim_end().chars().next_back().is_some_and(|c| c.is_ascii() && !c.is_ascii_control())
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
}
