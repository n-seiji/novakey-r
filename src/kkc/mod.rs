//! azooKey 互換のかな漢字変換 (Swift 版 AzooKeyKanaKanjiConverter v0.11.2 の Rust 移植)。
//!
//! 辞書ファイル形式 (`dict` / `louds`)、ラティスと N-best Viterbi (`lattice`)、動的エントリ (`wise`)、
//! 候補の組み立て (`result`) を持つ。IME から使うのは `RustEngine`。
//! 学習は `learning` (設定 `learning_enabled` (環境変数 `SHIZUKU_DISABLE_LEARNING=1` で false) で無効)。
#![forbid(unsafe_code)]

pub mod check;
pub mod compare;
pub mod dict;
pub mod elem;
pub mod lattice;
pub mod learning;
pub mod live_backspace;
pub mod louds;
pub mod result;
pub mod variants;
pub mod wise;

use std::cell::RefCell;
use std::collections::HashSet;
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

    /// カタカナの読み `katakana` で、表記が漢字 `kanji` 1 字の見出しが辞書にあるか。live 変換の Backspace が読みを切り分けるのに使う。
    pub fn has_kanji_reading(&self, katakana: &str, kanji: char) -> bool {
        let mut buf = [0u8; 4];
        let kanji: &str = kanji.encode_utf8(&mut buf);
        !katakana.is_empty()
            && self
                .dict
                .prefix_entries(katakana)
                .is_ok_and(|entries| entries.iter().any(|e| e.ruby == katakana && e.word == kanji))
    }

    /// カタカナの読みがちょうど一致する辞書の見出し (派生候補の連濁の語探し用)。学習・動的な語は見ない。辞書が読めなければ空。
    fn headwords(&self, katakana: &str) -> Vec<dict::Entry> {
        self.dict.prefix_entries(katakana).map(|v| v.into_iter().filter(|e| e.ruby == katakana).collect()).unwrap_or_default()
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

/// 全文候補の並べ替え。(文脈, 文字列, engine のスコア) -> 良い順の添字、使えなければ `None`。本番は `judge::rerank`。
type Judge = fn(&str, &[&str], &[f32]) -> Option<Vec<usize>>;

/// 2 つの文の比較。(文脈, 文, 比べる文) -> 前者の対数尤度 - 後者の対数尤度、使えなければ `None`。本番は `judge::margin`。
type Compare = fn(&str, &str, &str) -> Option<f64>;

/// `prefers_text` が英語を選ぶのに要る、審判の対数尤度の差。
const MARGIN: f64 = 4.0;

/// `rejects_english` が日本語の読みを選ぶのに要る、審判の対数尤度の差 (読み - 英字)。
/// 測った範囲では、英語として打つ語 (issue, line, rename…) は差が -1.3 以上、日本語の文節 (imago, anode, kiwano…) は -3 以下に分かれた。
const VETO_MARGIN: f64 = 2.0;

/// Rust 実装のかな漢字変換エンジン。
pub struct RustEngine {
    core: Option<Rc<KkcCore>>,
    judge: Judge,
    compare: Compare,
    /// 審判に渡す文脈 (アプリで直前に確定した文字列)。メモリ上だけで、ログにも書かない。
    context: String,
    /// 直近の変換の revision と、返した候補ごとの `Candidate.data`。`select` の検証と学習用。
    last: Option<(u64, Vec<Vec<Rc<elem::Elem>>>)>,
}

impl RustEngine {
    pub fn new() -> Self {
        Self { core: shared_core(), judge: crate::judge::rerank, compare: crate::judge::margin, context: String::new(), last: None }
    }

    #[cfg(test)]
    pub fn with_core(core: Rc<KkcCore>) -> Self {
        Self { core: Some(core), judge: crate::judge::rerank, compare: crate::judge::margin, context: String::new(), last: None }
    }

    /// 審判を差し替える (テスト用。実際の並べ替え経路を fake で通す)。
    #[cfg(test)]
    pub fn with_judge(mut self, judge: Judge) -> Self {
        self.judge = judge;
        self
    }

    /// 2 文の比較を差し替える (テスト用)。
    #[cfg(test)]
    pub fn with_compare(mut self, compare: Compare) -> Self {
        self.compare = compare;
        self
    }
}

/// `order` が `0..n` の並べ替えか (審判の返り値が壊れていても、候補が欠けたり重なったりしないように)。
fn is_permutation(order: &[usize], n: usize) -> bool {
    let mut seen = vec![false; n];
    order.len() == n && order.iter().all(|&k| k < n && !std::mem::replace(&mut seen[k], true))
}

/// 上位の全文候補から派生候補 (`variants`) を作り、最後の全文候補の直後に全文候補として足す。
/// 先頭の `top` (打ち切り済み) は動かさない。スコアは元の候補から `PENALTY` 引く。審判がなければ元の全文候補の後ろに並ぶ。
fn add_variants(core: &KkcCore, top: &mut Vec<result::Cand>) {
    let full: Vec<usize> = (0..top.len()).filter(|&i| top[i].source == result::Source::Full).collect();
    let Some(&last_full) = full.last() else {
        return;
    };
    let origins: Vec<&[Rc<elem::Elem>]> = full.iter().take(variants::ORIGINS).map(|&i| top[i].data.as_slice()).collect();
    let exclude: HashSet<String> = top.iter().map(|c| c.text.replace('\u{1F}', "")).collect();
    let made = variants::generate(&origins, &exclude, &|ruby| core.headwords(ruby));
    let cands: Vec<result::Cand> = made
        .into_iter()
        .map(|(k, v)| {
            let value = top[full[k]].value - variants::PENALTY;
            let ruby_count = v.data.iter().map(|e| e.ruby_len).sum();
            result::Cand { text: v.text, value, ruby_count, data: v.data, source: result::Source::Full }
        })
        .collect();
    top.splice(last_full + 1..last_full + 1, cands);
}

impl Default for RustEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl CandidateEngine for RustEngine {
    /// 全文候補 (Full) だけを審判で並べ替え、それぞれ元の枠に入れ直す。審判が使えなければ engine の順。
    /// `last` (学習用) も同じ並びで持つので、添字と学習データはずれない。
    fn convert(&mut self, revision: u64, hiragana: &str) -> Vec<String> {
        let Some(core) = &self.core else {
            return Vec::new();
        };
        if hiragana.is_empty() {
            return Vec::new();
        }
        let converted = core.convert_all(hiragana);
        let mut top: Vec<result::Cand> = converted.main[..converted.main.len().min(TOP_N)].to_vec();
        add_variants(core, &mut top);
        let mut order: Vec<usize> = (0..top.len()).collect();
        let slots: Vec<usize> = (0..top.len()).filter(|&i| top[i].source == result::Source::Full).collect();
        let texts: Vec<String> = slots.iter().map(|&i| top[i].text.replace('\u{1F}', "")).collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let scores: Vec<f32> = slots.iter().map(|&i| top[i].value).collect();
        if let Some(ranked) = (self.judge)(&self.context, &refs, &scores).filter(|r| is_permutation(r, slots.len())) {
            for (&slot, k) in slots.iter().zip(ranked) {
                order[slot] = slots[k];
            }
        }
        let texts: Vec<String> = order.iter().map(|&i| top[i].text.replace('\u{1F}', "")).collect();
        self.last = Some((revision, order.iter().map(|&i| top[i].data.clone()).collect()));
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

    fn set_context(&mut self, context: &str) {
        self.context.clear();
        self.context.push_str(context);
    }

    fn has_reading(&self, hiragana: &str) -> bool {
        self.core.as_ref().is_some_and(|core| core.has_reading(hiragana))
    }

    fn has_dictionary(&self) -> bool {
        self.core.is_some()
    }

    /// 文脈の末尾 (空白を除く) が ASCII のときだけ審判に聞く。文脈なしや日本語の文脈では、日本語として自然な語 (more → 漏れ) も
    /// 差が余裕を超えるので英語にしない。`claude ` のあとの code は 6.7、文脈なしの more は 7.2 で、差だけでは分けられない。
    /// 審判が使えない・失敗したときは false (engine の順のまま)。
    fn prefers_text(&mut self, text: &str, over: &str) -> bool {
        crate::context::ends_ascii(&self.context)
            && (self.compare)(&self.context, text, over).is_some_and(|diff| diff >= MARGIN)
    }

    /// 英字より読みの方が文として自然か。審判が使えない・失敗したときは false (英語のまま)。
    fn rejects_english(&mut self, raw: &str, reading: &str) -> bool {
        (self.compare)(&self.context, raw, reading).is_some_and(|diff| diff <= -VETO_MARGIN)
    }

    /// `last` を読むだけで取り出さない (`select` が学習に使う)。
    fn words(&self, revision: u64, index: usize) -> Option<Vec<(String, String)>> {
        let (r, candidates) = self.last.as_ref()?;
        if *r != revision {
            return None;
        }
        Some(candidates.get(index)?.iter().map(|e| (e.word.clone(), e.ruby.clone())).collect())
    }

    fn has_kanji_reading(&self, katakana: &str, kanji: char) -> bool {
        self.core.as_ref().is_some_and(|core| core.has_kanji_reading(katakana, kanji))
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

    fn reverse_judge(_context: &str, texts: &[&str], _scores: &[f32]) -> Option<Vec<usize>> {
        Some((0..texts.len()).rev().collect())
    }

    fn no_judge(_context: &str, _texts: &[&str], _scores: &[f32]) -> Option<Vec<usize>> {
        None
    }

    fn broken_judge(_context: &str, texts: &[&str], _scores: &[f32]) -> Option<Vec<usize>> {
        Some(vec![0; texts.len()])
    }

    fn full_slots(core: &KkcCore, reading: &str) -> Vec<usize> {
        let converted = core.convert_all(reading);
        let n = converted.main.len().min(TOP_N);
        (0..n).filter(|&i| converted.main[i].source == result::Source::Full).collect()
    }

    #[test]
    fn judge_reverses_only_the_full_slots_in_the_displayed_order() {
        let core = Rc::new(KkcCore::open_with_learning(Learning::in_memory()).unwrap());
        let slots = full_slots(&core, "かんじ");
        assert!(slots.len() >= 2, "前提: 全文候補が 2 つ以上 ({slots:?})");
        let base = RustEngine::with_core(core.clone()).with_judge(no_judge).convert(1, "かんじ");
        let shown = RustEngine::with_core(core).with_judge(reverse_judge).convert(1, "かんじ");
        assert_eq!(base.len(), shown.len());
        for (pos, &slot) in slots.iter().enumerate() {
            assert_eq!(shown[slot], base[slots[slots.len() - 1 - pos]], "全文候補の枠だけが逆順になる");
        }
        for i in (0..base.len()).filter(|i| !slots.contains(i)) {
            assert_eq!(shown[i], base[i], "全文以外の枠は動かない");
        }
        assert_ne!(shown, base);
    }

    #[test]
    fn selecting_a_judge_reordered_candidate_learns_that_candidate() {
        let core = Rc::new(KkcCore::open_with_learning(Learning::in_memory()).unwrap());
        let slots = full_slots(&core, "かんじ");
        let mut e = RustEngine::with_core(core.clone()).with_judge(reverse_judge);
        let shown = e.convert(1, "かんじ");
        // 審判で最後の全文候補が先頭の枠に来ている。それを選ぶ。
        let picked = shown[slots[0]].clone();
        assert_ne!(RustEngine::with_core(core.clone()).with_judge(no_judge).convert(9, "かんじ")[slots[0]], picked);
        assert!(e.select(1, slots[0]));
        // 審判なしで引き直すと、選んだ候補が (表示順ではなく中身で) 学習されて先頭に出る。
        let after = RustEngine::with_core(core).with_judge(no_judge).convert(2, "かんじ");
        assert_eq!(after[0], picked, "{after:?}");
    }

    #[test]
    fn a_broken_judge_result_keeps_the_engine_order() {
        let core = Rc::new(KkcCore::open_with_learning(Learning::in_memory()).unwrap());
        let base = RustEngine::with_core(core.clone()).with_judge(no_judge).convert(1, "かんじ");
        let shown = RustEngine::with_core(core).with_judge(broken_judge).convert(1, "かんじ");
        assert_eq!(shown, base);
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
    fn live_backspace_drops_only_the_reading_of_the_last_displayed_character() {
        let core = Rc::new(KkcCore::open_with_learning(Learning::in_memory()).unwrap());
        let cases = [
            ("せんせい", "先生", 2),
            ("がくせい", "学生", 2),
            ("じんせい", "人生", 2),
            ("かいしゃ", "会社", 2),
            ("じかん", "時間", 2),
            ("とうきょう", "東京", 3),
            ("かいしゃのけん", "会社の件", 2),
        ];
        for (reading, word, want) in cases {
            let mut e = RustEngine::with_core(core.clone()).with_judge(no_judge);
            let cands = e.convert(1, reading);
            let words = e.words(1, index_of(&cands, word)).unwrap();
            let words: Vec<(&str, &str)> = words.iter().map(|(w, r)| (w.as_str(), r.as_str())).collect();
            let dropped = live_backspace::drop_len(&words, reading, word, |s, k| e.has_kanji_reading(s, k));
            assert_eq!(dropped.map(|d| d.len), Some(want), "{word}");
        }
    }

    #[test]
    fn words_does_not_consume_the_learning_data() {
        let core = Rc::new(KkcCore::open_with_learning(Learning::in_memory()).unwrap());
        let mut e = RustEngine::with_core(core).with_judge(no_judge);
        let cands = e.convert(1, "かいしゃ");
        let i = index_of(&cands, "会社");
        assert!(e.words(1, i).is_some());
        assert!(e.select(1, i), "words の後も select できる");
    }

    #[test]
    fn words_is_none_for_a_stale_revision_or_an_unknown_index() {
        let core = Rc::new(KkcCore::open_with_learning(Learning::in_memory()).unwrap());
        let mut e = RustEngine::with_core(core).with_judge(no_judge);
        let cands = e.convert(1, "かいしゃ");
        let i = index_of(&cands, "会社");
        assert_eq!(e.words(2, i), None);
        assert_eq!(e.words(1, 99), None);
        assert_eq!(e.words(1, i), Some(vec![("会社".to_string(), "カイシャ".to_string())]));
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
        let dir = std::env::temp_dir().join(format!("shizuku-engine-learn-{}", std::process::id()));
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

    thread_local! {
        static SEEN: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    fn spy_judge(context: &str, _texts: &[&str], _scores: &[f32]) -> Option<Vec<usize>> {
        SEEN.with(|s| s.borrow_mut().push(context.to_string()));
        None
    }

    #[test]
    fn 設定した文脈が審判に渡り_空にすると文脈なしに戻る() {
        let core = Rc::new(KkcCore::open().unwrap());
        let mut e = RustEngine::with_core(core).with_judge(spy_judge);
        e.convert(1, "かんじ");
        e.set_context("机の");
        e.convert(2, "かんじ");
        e.set_context("");
        e.convert(3, "かんじ");
        assert_eq!(SEEN.with(|s| s.borrow().clone()), ["", "机の", ""]);
    }

    thread_local! {
        static COMPARED: RefCell<Vec<(String, String, String)>> = const { RefCell::new(Vec::new()) };
    }

    fn margin_of_four(context: &str, text: &str, over: &str) -> Option<f64> {
        COMPARED.with(|c| c.borrow_mut().push((context.to_string(), text.to_string(), over.to_string())));
        Some(MARGIN)
    }

    fn margin_below_four(_context: &str, _text: &str, _over: &str) -> Option<f64> {
        Some(MARGIN - 0.1)
    }

    fn no_margin(_context: &str, _text: &str, _over: &str) -> Option<f64> {
        None
    }

    #[test]
    fn 比べた差が余裕に届いたときだけ英語を選び_審判が使えなければ選ばない() {
        let core = Rc::new(KkcCore::open_with_learning(Learning::in_memory()).unwrap());
        let mut e = RustEngine::with_core(core).with_compare(margin_of_four);
        e.set_context("claude ");
        assert!(e.prefers_text("code", "子で"));
        assert_eq!(COMPARED.with(|c| c.borrow().clone()), [("claude ".to_string(), "code".to_string(), "子で".to_string())]);
        e = e.with_compare(margin_below_four);
        assert!(!e.prefers_text("code", "子で"));
        e = e.with_compare(no_margin);
        assert!(!e.prefers_text("code", "子で"));
    }

    #[test]
    fn 文脈が空か日本語で終わるときは審判に聞かず英語を選ばない() {
        let core = Rc::new(KkcCore::open_with_learning(Learning::in_memory()).unwrap());
        let mut e = RustEngine::with_core(core).with_compare(margin_of_four);
        let before = COMPARED.with(|c| c.borrow().len());
        for context in ["", "机の", "今日は "] {
            e.set_context(context);
            assert!(!e.prefers_text("more", "漏れ"), "{context:?}");
        }
        assert_eq!(COMPARED.with(|c| c.borrow().len()), before);
    }

    fn margin_minus_two(_context: &str, _text: &str, _over: &str) -> Option<f64> {
        Some(-VETO_MARGIN)
    }

    fn margin_minus_one(_context: &str, _text: &str, _over: &str) -> Option<f64> {
        Some(-VETO_MARGIN + 0.1)
    }

    #[test]
    fn 読みが余裕以上に自然なときだけ英字を退け_審判が使えなければ退けない() {
        let core = Rc::new(KkcCore::open_with_learning(Learning::in_memory()).unwrap());
        let mut e = RustEngine::with_core(core).with_compare(margin_minus_two);
        assert!(e.rejects_english("imago", "今後"));
        e = e.with_compare(margin_minus_one);
        assert!(!e.rejects_english("imago", "今後"));
        e = e.with_compare(no_margin);
        assert!(!e.rejects_english("imago", "今後"));
    }

    /// 派生候補 (一棟貸し) を後ろに置く審判。`want` を先頭に、残りは元の順。
    fn prefer_variant_judge(_context: &str, texts: &[&str], _scores: &[f32]) -> Option<Vec<usize>> {
        let k = texts.iter().position(|t| *t == "一棟貸し")?;
        let mut order: Vec<usize> = (0..texts.len()).filter(|&i| i != k).collect();
        order.insert(0, k);
        Some(order)
    }

    #[test]
    fn 派生候補は全文候補の後ろに足され_審判なしでは元の全文候補が先に並ぶ() {
        let core = Rc::new(KkcCore::open_with_learning(Learning::in_memory()).unwrap());
        let base = core.convert_all("いっとうがし");
        let n_full = base.main.iter().take(TOP_N).filter(|c| c.source == result::Source::Full).count();
        let mut e = RustEngine::with_core(core).with_judge(no_judge);
        let cands = e.convert(1, "いっとうがし");
        let shown_base: Vec<String> = base.main.iter().take(TOP_N).map(|c| c.text.replace('\u{1F}', "")).collect();
        let i = index_of(&cands, "一棟貸し");
        assert!(i >= n_full, "派生候補は元の全文候補より前に出ない");
        assert_eq!(cands[..n_full], shown_base[..n_full]);
        let added = cands.len() - shown_base.len();
        assert!(added >= 1 && added <= variants::MAX_VARIANTS, "{added}");
        assert_eq!(cands[n_full + added..], shown_base[n_full..], "全文以外の候補は押し出されない");
    }

    #[test]
    fn 派生候補の語列は表記も読みも表示と読みに一致する() {
        let core = Rc::new(KkcCore::open_with_learning(Learning::in_memory()).unwrap());
        let mut e = RustEngine::with_core(core).with_judge(no_judge);
        let cands = e.convert(1, "べっそうのいっとうがし");
        let i = index_of(&cands, "別荘の一棟貸し");
        let words = e.words(1, i).unwrap();
        assert_eq!(words.iter().map(|(w, _)| w.as_str()).collect::<String>(), cands[i]);
        assert_eq!(words.iter().map(|(_, r)| r.as_str()).collect::<String>(), "ベッソウノイットウガシ");
        assert!(words.contains(&("一棟".to_string(), "イットウ".to_string())));
        assert!(words.contains(&("貸し".to_string(), "ガシ".to_string())));
        assert_eq!(e.words(2, i), None, "古い revision は None");
    }

    #[test]
    fn 数詞と助数詞の派生はライブのbackspaceで末尾の字の読みだけを落とす() {
        let core = Rc::new(KkcCore::open_with_learning(Learning::in_memory()).unwrap());
        let cases = [("いっとうがし", "一棟貸し", 1), ("いっとう", "一棟", 2)];
        for (reading, word, want) in cases {
            let mut e = RustEngine::with_core(core.clone()).with_judge(no_judge);
            let cands = e.convert(1, reading);
            let words = e.words(1, index_of(&cands, word)).unwrap();
            let words: Vec<(&str, &str)> = words.iter().map(|(w, r)| (w.as_str(), r.as_str())).collect();
            let dropped = live_backspace::drop_len(&words, reading, word, |s, k| e.has_kanji_reading(s, k));
            assert_eq!(dropped.map(|d| d.len), Some(want), "{word}");
        }
    }

    #[test]
    fn 審判が選んだ派生候補を確定すると学習され_審判なしでも次は先頭に出る() {
        let core = Rc::new(KkcCore::open_with_learning(Learning::in_memory()).unwrap());
        let mut e = RustEngine::with_core(core.clone()).with_judge(prefer_variant_judge);
        let shown = e.convert(1, "いっとうがし");
        assert_eq!(shown[0], "一棟貸し");
        let before = RustEngine::with_core(core.clone()).with_judge(no_judge).convert(5, "いっとうがし");
        assert_ne!(before[0], "一棟貸し");
        assert!(e.select(1, 0));
        let after = RustEngine::with_core(core).with_judge(no_judge).convert(2, "いっとうがし");
        assert_eq!(after[0], "一棟貸し", "{after:?}");
    }
}
