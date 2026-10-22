#![forbid(unsafe_code)]

// english_memory.rs - 英語として確定した語 (git など) を覚え、次から打ちかけの時点で英字表示するための記憶。
// 置き場所は変換の学習と同じディレクトリの english.txt。1 行 1 語で、新しく使った語が先頭。
// 新しい語を覚えたときだけ書き出す (語はたまにしか増えないので、変換の学習のような裏の書き手は持たない)。

use std::cell::RefCell;
use std::io::Write;
use std::path::{Path, PathBuf};

/// 覚えておく語数の上限。超えたら使っていない順に忘れる。
const CAPACITY: usize = 2048;
/// 覚える語の長さ。長い英字列は合言葉やトークンの手打ちかもしれないので覚えない。
const MAX_LEN: usize = 24;
const FILE_NAME: &str = "english.txt";

/// 覚えてよい語か。小文字の英字だけで 2 文字以上 `MAX_LEN` 以下。
pub fn is_memorable(word: &str) -> bool {
    (2..=MAX_LEN).contains(&word.len()) && word.bytes().all(|b| b.is_ascii_lowercase())
}

/// 1 行 1 語を読む。覚えてよくない行と重複は捨て、上限で切る。
pub fn parse(text: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    for line in text.lines().map(str::trim) {
        if is_memorable(line) && !words.iter().any(|w| w == line) {
            words.push(line.to_string());
        }
    }
    words.truncate(CAPACITY);
    words
}

/// word を先頭へ移した新しい並びを返す。新しい語なら末尾を上限で切る。
pub fn touched(words: &[String], word: &str) -> Vec<String> {
    std::iter::once(word.to_string())
        .chain(words.iter().filter(|w| *w != word).cloned())
        .take(CAPACITY)
        .collect()
}

struct Store {
    path: Option<PathBuf>,
    words: Vec<String>,
}

impl Store {
    fn from_env() -> Self {
        let disabled = std::env::var("NOVAKEY_DISABLE_LEARNING").is_ok_and(|v| v == "1");
        let path = (!disabled)
            .then(crate::kkc::learning_dir)
            .flatten()
            .map(|dir| dir.join(FILE_NAME));
        let words = path
            .as_deref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| parse(&s))
            .unwrap_or_default();
        Self { path, words }
    }
}

thread_local! {
    static STORE: RefCell<Option<Store>> = const { RefCell::new(None) };
}

fn with_store<T>(f: impl FnOnce(&mut Store) -> T) -> T {
    STORE.with(|cell| f(cell.borrow_mut().get_or_insert_with(Store::from_env)))
}

/// 英語として確定したことのある語か。
pub fn knows(word: &str) -> bool {
    with_store(|s| s.words.iter().any(|w| w == word))
}

/// 英語として確定した語を覚える。パスワード欄やパスワード管理アプリでは覚えない。
pub fn learn(word: &str) {
    if !is_memorable(word) || !crate::kkc::learning_allowed() {
        return;
    }
    with_store(|s| {
        let Some(path) = s.path.clone() else { return };
        let is_new = !s.words.iter().any(|w| w == word);
        s.words = touched(&s.words, word);
        if is_new {
            save(&path, &s.words);
        }
    });
}

fn save(path: &Path, words: &[String]) {
    let result = (|| -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            crate::private_fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension(format!("txt.tmp-{}", std::process::id()));
        let written = (|| -> std::io::Result<()> {
            let mut file = crate::private_fs::create_file(&tmp)?;
            file.write_all((words.join("\n") + "\n").as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&tmp, path)
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        written
    })();
    if let Err(e) = result {
        crate::platform::log(&format!("english memory not saved: {e}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 覚えるのは小文字の英字だけの2文字以上の語() {
        assert!(is_memorable("git"));
        assert!(is_memorable("ls"));
        assert!(!is_memorable("k"));
        assert!(!is_memorable("Git"));
        assert!(!is_memorable("a1"));
        assert!(!is_memorable(&"a".repeat(MAX_LEN + 1)));
    }

    #[test]
    fn 読むときは覚えられない行と重複を捨てる() {
        assert_eq!(parse("git\n\nGit\ngit\nnpm\nx\n"), ["git", "npm"]);
    }

    #[test]
    fn 使った語は先頭に移り上限を超えた古い語は忘れる() {
        let full: Vec<String> = (0..CAPACITY).map(|i| format!("{}{}", "ab".repeat(i / 26 + 1), (b'a' + (i % 26) as u8) as char)).collect();
        let after = touched(&full, "git");
        assert_eq!(after.len(), CAPACITY);
        assert_eq!(after[0], "git");
        assert_eq!(after[1], full[0]);
        assert!(!after.contains(full.last().unwrap()));
        assert_eq!(touched(&["npm".into(), "git".into()], "git"), ["git", "npm"]);
    }
}
