//! azooKey 互換のかな漢字変換 (Swift 版 AzooKeyKanaKanjiConverter v0.11.2 の Rust 移植)。
//!
//! 辞書ファイル形式 (`dict` / `louds`)、ラティスと N-best Viterbi (`lattice`)、動的エントリ (`wise`)、
//! 候補の組み立て (`result`) を持つ。IME から使うのは `RustEngine`。
//! 学習は `learning` (設定 `learning_enabled` (環境変数 `NOVAKEY_DISABLE_LEARNING=1` で false) で無効)。
#![forbid(unsafe_code)]

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

pub use dict::{DictError, Dictionary};
pub use result::Converted;

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

    #[cfg(test)]
    pub fn learning(&self) -> &RefCell<learning::Learning> {
        &self.learning
    }

    /// 確定した候補のデータを学習する。
    pub fn learn(&self, data: &[Rc<elem::Elem>]) {
        self.learning.borrow_mut().learn(data, &|c| self.dict.char_id(c) != 255);
    }

    /// 読みがそのまま辞書の見出しにあるか。トライを 1 本たどるだけで、ラティスは作らない。学習・動的な語は見ない。
    pub fn has_reading(&self, hiragana: &str) -> bool {
        let katakana = dict::to_katakana(hiragana);
        !katakana.is_empty()
            && self.dict.prefix_entries(&katakana).is_ok_and(|entries| entries.iter().any(|e| e.ruby == katakana))
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

/// 学習データの置き場所。英語の記憶 (english_memory) も同じ場所に置く。
pub fn learning_dir(config: &crate::config::Config) -> Option<std::path::PathBuf> {
    learning::default_dir(config)
}

/// 辞書を開けなかったときに、次に開き直すまで空ける時間。コントローラ (入力クライアント) が増えるたびに
/// 開き直すと、辞書が無い環境で毎回ディスクを探しログを出すので、この間は結果 (失敗) を使い回す。
/// 辞書を入れ直したときは IME が再起動するので、長く空けても困らない。
const REOPEN_AFTER_FAILURE: std::time::Duration = std::time::Duration::from_secs(300);

/// 共有コアを持つ入れ物。失敗した時刻を覚え、`REOPEN_AFTER_FAILURE` の間は開き直さず、失敗のログは 1 度だけ。
struct CoreSlot<T> {
    core: Option<Rc<T>>,
    failed_at: Option<std::time::Instant>,
    logged: bool,
}

impl<T> CoreSlot<T> {
    const fn new() -> Self {
        Self { core: None, failed_at: None, logged: false }
    }

    fn get(
        &mut self,
        now: std::time::Instant,
        open: impl FnOnce() -> Result<T, String>,
        log: impl FnOnce(&str),
    ) -> Option<Rc<T>> {
        if self.core.is_none() {
            if let Some(failed) = self.failed_at {
                if now.saturating_duration_since(failed) < REOPEN_AFTER_FAILURE {
                    return None;
                }
            }
            match open() {
                Ok(core) => {
                    self.core = Some(Rc::new(core));
                    self.failed_at = None;
                }
                Err(e) => {
                    self.failed_at = Some(now);
                    if !std::mem::replace(&mut self.logged, true) {
                        log(&format!("dictionary unavailable; conversion falls back to kana: {e}"));
                    }
                    return None;
                }
            }
        }
        self.core.clone()
    }
}

thread_local! {
    static CORE: RefCell<CoreSlot<KkcCore>> = const { RefCell::new(CoreSlot::new()) };
}

/// スレッドごとの共有コア。開けなかったときは `None` (ログは 1 度だけ。再試行は `REOPEN_AFTER_FAILURE` 以上空ける)。
/// エンジンが `None` でも、composer はかな候補で変換を続けられる。
pub fn shared_core() -> Option<Rc<KkcCore>> {
    CORE.with(|c| {
        c.borrow_mut().get(
            std::time::Instant::now(),
            || KkcCore::open_with_learning(learning::Learning::from_config(crate::config::get())).map_err(|e| e.to_string()),
            crate::platform::log,
        )
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

    #[cfg(test)]
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
    /// 学習の可否は呼び出し側 (Composer::commit) で判定済みの前提。直接呼ぶとゲートを素通りする。
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

    fn has_reading(&self, hiragana: &str) -> bool {
        self.core.as_ref().is_some_and(|core| core.has_reading(hiragana))
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
    fn has_reading_is_true_only_for_an_exact_dictionary_headword() {
        let e = engine(Learning::in_memory());
        assert!(e.has_reading("まで"));
        assert!(e.has_reading("さめ"));
        assert!(!e.has_reading("こっみと"));
        assert!(!e.has_reading("まで\u{3042}\u{3042}\u{3042}x"));
        assert!(!e.has_reading(""));
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
    fn a_failed_open_is_cached_logged_once_and_retried_only_after_the_interval() {
        use std::cell::{Cell, RefCell};
        use std::time::{Duration, Instant};
        let opens = Cell::new(0);
        let logs = RefCell::new(Vec::new());
        let mut slot: CoreSlot<u32> = CoreSlot::new();
        let t0 = Instant::now();
        let get = |slot: &mut CoreSlot<u32>, at: Instant, ok: bool| {
            slot.get(
                at,
                || {
                    opens.set(opens.get() + 1);
                    if ok { Ok(7) } else { Err("missing".to_string()) }
                },
                |m| logs.borrow_mut().push(m.to_string()),
            )
        };
        assert!(get(&mut slot, t0, false).is_none());
        for i in 1..5 {
            assert!(get(&mut slot, t0 + Duration::from_secs(i), false).is_none());
        }
        assert_eq!(opens.get(), 1, "間隔内は開き直さない");
        assert!(get(&mut slot, t0 + REOPEN_AFTER_FAILURE, false).is_none());
        assert_eq!(opens.get(), 2);
        assert_eq!(logs.borrow().len(), 1, "ログは 1 度だけ");
        assert!(!logs.borrow()[0].contains("かんじ"));
        let core = get(&mut slot, t0 + REOPEN_AFTER_FAILURE * 2, true).expect("開けたら使える");
        assert_eq!(*core, 7);
        assert!(get(&mut slot, t0 + REOPEN_AFTER_FAILURE * 2, false).is_some(), "開けたあとは開き直さない");
        assert_eq!(opens.get(), 3);
    }

    #[test]
    fn selecting_a_candidate_learns_it() {
        let mut e = engine(Learning::in_memory());
        let cands = e.convert(1, "かんじ");
        assert!(e.core.as_ref().unwrap().learning().borrow().is_empty());
        assert!(e.select(1, index_of(&cands, "幹事")));
        assert!(!e.core.as_ref().unwrap().learning().borrow().is_empty());
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
