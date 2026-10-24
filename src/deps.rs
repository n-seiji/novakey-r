#![forbid(unsafe_code)]

// deps.rs - Composer / Session が外に求める語彙と記憶の口。本番は既存の表とファイルを包むだけ。
// 学習してよいかの判定は Session が `Gates::learning` で 1 か所だけ行う。EnglishMemory::learn を直接呼ぶとそのゲートを素通りする。

/// 読みから引く語彙。引くだけで状態を変えない。
pub trait Lexicon {
    /// カタカナ語に対応する英単語。
    fn english_for(&self, katakana: &str) -> Vec<String>;
    /// 打鍵列が英単語か。
    fn is_english_word(&self, typed: &str) -> bool;
    /// 英語として確定したことのある語か (EnglishMemory が覚えたもの)。
    fn knows_english(&self, word: &str) -> bool;
    /// ひらがなの読み全体に対応する絵文字。
    fn emoji_for(&self, hiragana: &str) -> Vec<String>;
}

/// 英語として確定した語の記憶への書き込み口。
pub trait EnglishMemory {
    fn learn(&mut self, word: &str);
}

pub struct ProdLexicon;

impl Lexicon for ProdLexicon {
    fn english_for(&self, katakana: &str) -> Vec<String> {
        crate::loanword::english_for(katakana)
    }
    fn is_english_word(&self, typed: &str) -> bool {
        crate::loanword::is_english_word(typed)
    }
    fn knows_english(&self, word: &str) -> bool {
        crate::english_memory::knows(word)
    }
    fn emoji_for(&self, hiragana: &str) -> Vec<String> {
        crate::emoji::emoji_for(hiragana)
    }
}

pub struct ProdMemory;

impl EnglishMemory for ProdMemory {
    fn learn(&mut self, word: &str) {
        crate::english_memory::learn(word);
    }
}

#[cfg(test)]
pub use fakes::{FakeLexicon, FakeMemory};

#[cfg(test)]
mod fakes {
    use super::{EnglishMemory, Lexicon};
    use std::cell::RefCell;
    use std::rc::Rc;

    /// 何も引けない語彙。差し替えたい口だけ builder で足す。`learned` は FakeMemory と共有する。
    #[derive(Clone)]
    pub struct FakeLexicon {
        english: fn(&str) -> Vec<String>,
        english_word: fn(&str) -> bool,
        emoji: fn(&str) -> Vec<String>,
        learned: Rc<RefCell<Vec<String>>>,
    }

    impl Default for FakeLexicon {
        fn default() -> Self {
            Self {
                english: |_| Vec::new(),
                english_word: |_| false,
                emoji: |_| Vec::new(),
                learned: Rc::default(),
            }
        }
    }

    impl FakeLexicon {
        pub fn with_english(mut self, f: fn(&str) -> Vec<String>) -> Self {
            self.english = f;
            self
        }
        pub fn with_english_word(mut self, f: fn(&str) -> bool) -> Self {
            self.english_word = f;
            self
        }
        pub fn with_emoji(mut self, f: fn(&str) -> Vec<String>) -> Self {
            self.emoji = f;
            self
        }
        /// この語彙が knows_english で引く記憶に書き込む FakeMemory。
        pub fn memory(&self) -> FakeMemory {
            FakeMemory { learned: self.learned.clone() }
        }
    }

    impl Lexicon for FakeLexicon {
        fn english_for(&self, katakana: &str) -> Vec<String> {
            (self.english)(katakana)
        }
        fn is_english_word(&self, typed: &str) -> bool {
            (self.english_word)(typed)
        }
        fn knows_english(&self, word: &str) -> bool {
            self.learned.borrow().iter().any(|w| w == word)
        }
        fn emoji_for(&self, hiragana: &str) -> Vec<String> {
            (self.emoji)(hiragana)
        }
    }

    #[derive(Clone, Default)]
    pub struct FakeMemory {
        learned: Rc<RefCell<Vec<String>>>,
    }

    impl FakeMemory {
        pub fn learned(&self) -> Vec<String> {
            self.learned.borrow().clone()
        }
    }

    impl EnglishMemory for FakeMemory {
        fn learn(&mut self, word: &str) {
            self.learned.borrow_mut().push(word.to_string());
        }
    }
}
