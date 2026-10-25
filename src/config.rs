//! プロセスの設定。環境変数はここ (`Config::from_env`) でだけ読み、起動時に `init` で一度だけ渡す。
//! 各モジュールは `get()` で読むだけで、環境変数には触らない。
#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// 学習データの親ディレクトリ (`SHIZUKU_MEMORY_DIR`、無ければ `~/Library/Application Support/Shizuku`)。
    /// HOME も無ければ `None` で、学習は読み書きしない。
    pub memory_dir: Option<PathBuf>,
    /// 学習・英語の記憶を使うか (`SHIZUKU_DISABLE_LEARNING=1` で false)。
    pub learning_enabled: bool,
    /// カタカナ語 -> 英語の表 (`SHIZUKU_LOANWORDS`、無ければアプリバンドルの Resources)。
    pub loanwords_path: Option<PathBuf>,
    /// 開発用語の英単語の一覧 (`SHIZUKU_DEV_WORDS`、無ければアプリバンドルの Resources)。
    pub dev_words_path: Option<PathBuf>,
    /// 読み -> 絵文字の表 (`SHIZUKU_EMOJI`、無ければアプリバンドルの Resources)。
    pub emoji_path: Option<PathBuf>,
    /// ライブ変換の審判モデルの置き場所 (`SHIZUKU_JUDGE_DIR`、無ければアプリバンドルの Resources/judge)。
    pub judge_dir: Option<PathBuf>,
}

impl Config {
    /// 何も読み書きしない設定 (`init` されていないテストの既定)。
    pub fn inert() -> Self {
        Self { memory_dir: None, learning_enabled: false, loanwords_path: None, dev_words_path: None, emoji_path: None, judge_dir: None }
    }

    pub fn from_env() -> Self {
        let memory_dir = match std::env::var("SHIZUKU_MEMORY_DIR") {
            Ok(d) if !d.is_empty() => Some(PathBuf::from(d)),
            _ => std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support/Shizuku")),
        };
        Self {
            memory_dir,
            learning_enabled: !std::env::var("SHIZUKU_DISABLE_LEARNING").is_ok_and(|v| v == "1"),
            loanwords_path: std::env::var_os("SHIZUKU_LOANWORDS").map(PathBuf::from).or_else(|| bundled("loanwords.tsv")),
            dev_words_path: std::env::var_os("SHIZUKU_DEV_WORDS").map(PathBuf::from).or_else(|| bundled("dev_words.txt")),
            emoji_path: std::env::var_os("SHIZUKU_EMOJI").map(PathBuf::from).or_else(|| bundled("emoji.tsv")),
            judge_dir: std::env::var_os("SHIZUKU_JUDGE_DIR").map(PathBuf::from).or_else(|| bundled("judge")),
        }
    }
}

/// Contents/MacOS/shizuku から見た Contents/Resources/<name>
fn bundled(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.parent()?.join("Resources").join(name))
}

static CONFIG: OnceLock<Config> = OnceLock::new();
static INERT: OnceLock<Config> = OnceLock::new();

/// 起動時に一度だけ呼ぶ。2 回目以降は無視する。
pub fn init(config: Config) {
    let _ = CONFIG.set(config);
}

/// `init` 済みならその設定、まだなら `inert()` (ユニットテストが環境や HOME に触れないように)。
pub fn get() -> &'static Config {
    CONFIG.get().unwrap_or_else(|| {
        // init 前に読んだ値は LazyLock の表や学習ストアに残り、後から init しても直らない。黙って無効にならないよう 1 度だけ残す。
        #[cfg(not(test))]
        {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| crate::platform::log("config: read before init; learning and tables are disabled"));
        }
        INERT.get_or_init(Config::inert)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_されていない間は何も読み書きしない設定を返す() {
        let c = get();
        assert!(!c.learning_enabled);
        assert!(c.memory_dir.is_none() && c.loanwords_path.is_none() && c.dev_words_path.is_none() && c.emoji_path.is_none());
    }

    #[test]
    fn 学習データの置き場所は設定の_memory_dir_の下になる() {
        let c = Config { memory_dir: Some(PathBuf::from("/tmp/m")), ..Config::inert() };
        assert_eq!(crate::kkc::learning_dir(&c), Some(PathBuf::from("/tmp/m/rust-learning")));
        assert_eq!(crate::kkc::learning_dir(&Config::inert()), None);
    }
}
