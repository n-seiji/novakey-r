//! azooKey 互換のかな漢字変換 (Swift 版 AzooKeyKanaKanjiConverter v0.11.2 の Rust 移植)。
//!
//! 辞書ファイル形式 (`dict` / `louds`)、ラティスと N-best Viterbi (`lattice`)、動的エントリ (`wise`)、
//! 候補の組み立て (`result`) を持つ。IME から使うのは `RustEngine`。
//! 学習は `learning` (環境変数 `NOVAKEY_DISABLE_LEARNING=1` で無効)。
#![forbid(unsafe_code)]
#![allow(dead_code, unused_imports)]

pub mod check;
pub mod compare;
pub mod dict;
pub mod elem;
pub mod lattice;
pub mod learning;
pub mod louds;
pub mod result;
pub mod wise;

use std::cell::RefCell;
use std::rc::Rc;

use crate::composer::CandidateEngine;

pub use dict::{DictError, Dictionary, Entry};
pub use result::{Cand, Converted, Source};

/// 保持する N-best の数 (本番の `ConvertRequestOptions.N_best`)
const N_BEST: usize = 9;
/// IME に返す候補の最大数
const TOP_N: usize = 9;

/// 辞書と辞書引きのメモ。コントローラ (入力クライアント) 間で共有する。
pub struct KkcCore {
    dict: Dictionary,
    cache: RefCell<lattice::LookupCache>,
    learning: RefCell<learning::Learning>,
}

impl KkcCore {
    /// 学習なしで開く (差分テストや計測は学習の影響を受けない)。
    pub fn open() -> Result<Self, DictError> {
        Self::open_with_learning(learning::Learning::disabled())
    }

    pub fn open_with_learning(learning: learning::Learning) -> Result<Self, DictError> {
        Ok(Self {
            dict: Dictionary::open_default()?,
            cache: RefCell::new(lattice::LookupCache::new()),
            learning: RefCell::new(learning),
        })
    }

    pub fn learning(&self) -> &RefCell<learning::Learning> {
        &self.learning
    }

    /// 確定した候補のデータを学習する。
    pub fn learn(&self, data: &[Rc<elem::Elem>]) {
        self.learning.borrow_mut().learn(data, &|c| self.dict.char_id(c) != 255);
    }

    pub fn dictionary(&self) -> &Dictionary {
        &self.dict
    }

    /// 読み全体を変換する。結果の全件 (`mainResults` / `firstClauseResults`)。毎回ラティスを作り直す。
    pub fn convert_all(&self, reading: &str) -> Converted {
        if reading.is_empty() {
            return Converted::default();
        }
        let chars = lattice::katakana_chars(reading);
        let katakana: String = chars.iter().collect();
        let learning = self.learning.borrow();
        let mut lat = lattice::Lattice::build_with_memory(&self.dict, &mut self.cache.borrow_mut(), Some(&learning), &chars);
        lat.run(&self.dict, N_BEST);
        result::process(&self.dict, &lat, &katakana)
    }
}

thread_local! {
    static CORE: RefCell<Option<Rc<KkcCore>>> = const { RefCell::new(None) };
}

/// スレッドごとの共有コア。開けなかったときはエラーを 1 度表示して `None`。
pub fn shared_core() -> Option<Rc<KkcCore>> {
    CORE.with(|c| {
        let mut slot = c.borrow_mut();
        if slot.is_none() {
            match KkcCore::open_with_learning(learning::Learning::from_env()) {
                Ok(core) => *slot = Some(Rc::new(core)),
                Err(e) => {
                    eprintln!("novakey_r: 辞書を開けませんでした: {e}");
                    return None;
                }
            }
        }
        slot.clone()
    })
}

/// Rust 実装のかな漢字変換エンジン。
pub struct RustEngine {
    core: Option<Rc<KkcCore>>,
    /// 直近の変換の revision と、返した候補ごとの `Candidate.data`。`select` の検証と学習用。
    last: Option<(u64, Vec<Vec<Rc<elem::Elem>>>)>,
}

impl RustEngine {
    pub fn new() -> Self {
        Self { core: shared_core(), last: None }
    }

    pub fn with_core(core: Rc<KkcCore>) -> Self {
        Self { core: Some(core), last: None }
    }
}

impl Default for RustEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl CandidateEngine for RustEngine {
    fn convert(&mut self, revision: u64, hiragana: &str) -> Vec<String> {
        let Some(core) = &self.core else {
            return Vec::new();
        };
        if hiragana.is_empty() {
            return Vec::new();
        }
        let converted = core.convert_all(hiragana);
        let top = &converted.main[..converted.main.len().min(TOP_N)];
        let texts: Vec<String> = top.iter().map(|c| c.text.replace('\u{1F}', "")).collect();
        self.last = Some((revision, top.iter().map(|c| c.data.clone()).collect()));
        texts
    }

    /// revision と添字を確かめて、選ばれた候補を学習する (保存まで行う)。
    fn select(&mut self, revision: u64, engine_index: usize) -> bool {
        let last = self.last.take();
        let (Some(core), Some((r, candidates))) = (&self.core, last) else {
            return false;
        };
        match candidates.get(engine_index) {
            Some(data) if r == revision => {
                core.learn(data);
                true
            }
            _ => false,
        }
    }

    fn cancel(&mut self) {
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::learning::Learning;
    use super::*;

    fn engine(learning: Learning) -> RustEngine {
        RustEngine::with_core(Rc::new(KkcCore::open_with_learning(learning).unwrap()))
    }

    /// 上位 9 件のうち `want` の添字。
    fn index_of(cands: &[String], want: &str) -> usize {
        cands.iter().position(|c| c == want).unwrap_or_else(|| panic!("{want} が候補にない: {cands:?}"))
    }

    #[test]
    fn selected_lower_candidate_comes_first_next_time() {
        let mut e = engine(Learning::in_memory());
        let before = e.convert(1, "かんじ");
        let i = index_of(&before, "幹事");
        assert!(i > 0, "前提: 幹事は最上位ではない ({before:?})");
        assert!(e.select(1, i));
        let after = e.convert(2, "かんじ");
        assert_eq!(after[0], "幹事", "{after:?}");
    }

    #[test]
    fn learned_word_also_helps_inside_a_longer_reading() {
        let mut e = engine(Learning::in_memory());
        let before = e.convert(1, "かんじ");
        assert!(e.select(1, index_of(&before, "幹事")));
        let after = e.convert(2, "かんじをかく");
        assert!(after[0].starts_with("幹事"), "{after:?}");
    }

    #[test]
    fn select_with_stale_revision_or_index_learns_nothing() {
        let mut e = engine(Learning::in_memory());
        let cands = e.convert(5, "かんじ");
        assert!(!e.select(6, index_of(&cands, "幹事")));
        e.convert(7, "かんじ");
        assert!(!e.select(7, 99));
        assert!(e.core.as_ref().unwrap().learning().borrow().is_empty());
        assert!(!e.select(7, 0), "select のあとは状態が捨てられる");
    }

    #[test]
    fn learning_survives_reopening_the_file() {
        let dir = std::env::temp_dir().join(format!("novakey-r-engine-learn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut e = engine(Learning::open(&dir));
        let before = e.convert(1, "かんじ");
        assert!(e.select(1, index_of(&before, "幹事")));
        e.core.as_ref().unwrap().learning().borrow().flush();
        let mut e2 = engine(Learning::open(&dir));
        assert_eq!(e2.convert(1, "かんじ")[0], "幹事");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (読み, 確定する候補の順位 (1 始まり), 確定する回数)。事例は 1 つの記憶に積み上げる。
    const LEARNING_CASES: &[(&str, usize, usize)] = &[
        ("かんじ", 3, 1),
        ("きかい", 2, 1),
        ("こうえん", 3, 1),
        ("せいかく", 2, 1),
        ("はし", 4, 1),
        ("かみ", 3, 1),
        ("あめ", 2, 1),
        ("いし", 3, 1),
        ("きょうはいいてんきですね", 3, 1),
        ("わたしのなまえはなかのです", 2, 1),
        ("あしたかいしゃにいきます", 2, 1),
        ("けんきゅうしつ", 3, 1),
        ("ふくざつなもんだい", 2, 1),
        ("おんがくをきく", 3, 1),
        ("しゅうまつにかいものにいく", 2, 1),
        ("はなをみる", 3, 2),
        ("せいふのほうしん", 2, 1),
        ("さくじつのかいぎ", 3, 1),
        ("しょうがっこう", 4, 1),
        ("そうだんにのってください", 2, 2),
        ("ほんをよむ", 3, 1),
        ("かてい", 3, 1),
    ];

    #[test]
    fn confirmed_candidates_rise_when_accumulated() {
        let mut e = engine(Learning::in_memory());
        let mut rev = 0;
        for &(reading, rank, times) in LEARNING_CASES {
            rev += 1;
            let before = e.convert(rev, reading);
            let picked = before[rank - 1].clone();
            for _ in 0..times {
                rev += 1;
                let list = e.convert(rev, reading);
                let i = index_of(&list, &picked);
                assert!(e.select(rev, i));
            }
            rev += 1;
            let after = e.convert(rev, reading);
            let now = index_of(&after, &picked);
            assert!(now < rank - 1, "{reading}: {before:?} -> {after:?}");
            e.cancel();
        }
    }

    #[test]
    fn disabled_learning_changes_nothing() {
        let mut e = engine(Learning::disabled());
        let before = e.convert(1, "かんじ");
        assert!(e.select(1, index_of(&before, "幹事")));
        assert_eq!(e.convert(2, "かんじ"), before);
    }
}
