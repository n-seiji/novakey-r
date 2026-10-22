//! 辞書要素 (`DicdataElement`) と、品詞 ID まわりの定数・判定 (`CIDData` / `MIDData` / `DicdataStore.isClause`)。
#![forbid(unsafe_code)]

use std::sync::OnceLock;

use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

pub const CID_BOS: usize = 0;
pub const CID_SYMBOL: usize = 5;
pub const CID_PROPER_NOUN: usize = 1288;
pub const CID_NUMBER: usize = 1295;
pub const CID_EOS: usize = 1316;
pub const MID_BOS_EOS: usize = 500;
pub const MID_GENERAL: usize = 501;
pub const MID_NUMBER: usize = 452;
pub const MID_SMALL_NUMBER: usize = 361;

/// この値以下のスコアを持つエントリは無視する (`DicdataStore.threshold`)
const THRESHOLD: f32 = -17.0;

/// 結合文字・正規化の影響を受けない (1 文字 = 1 grapheme、NFC で不変) と分かっている文字
fn is_simple_char(c: char) -> bool {
    let u = c as u32;
    u < 0x300 || (0x3041..=0x3096).contains(&u) || (0x30A1..=0x30FA).contains(&u) || (0x30FC..=0x30FF).contains(&u) || (0x4E00..=0x9FFF).contains(&u)
}

/// Swift の `String.count` (grapheme cluster 数)。かな・漢字・ASCII だけなら文字数と同じなので速い道を通す。
pub fn count_graphemes(s: &str) -> usize {
    // \r\n は 1 grapheme
    if s.chars().all(is_simple_char) && !s.contains('\r') {
        s.chars().count()
    } else {
        s.graphemes(true).count()
    }
}

/// Swift の `String` の ==/hash は正準等価で比べる (互換漢字 U+FA19 と U+795E は同じ文字列)。
/// 重複排除のキーはこの正規化 (NFC) した文字列を使う。
pub fn canonical_key(s: &str) -> String {
    if s.chars().all(is_simple_char) {
        s.to_string()
    } else {
        s.nfc().collect()
    }
}

/// 辞書の 1 要素。`ruby` はカタカナ。
#[derive(Debug, Clone)]
pub struct Elem {
    pub word: String,
    pub ruby: String,
    pub lcid: usize,
    pub rcid: usize,
    pub mid: usize,
    pub base: f32,
    pub adjust: f32,
    /// `word.count` (grapheme 数)
    pub word_len: usize,
    /// `ruby.count` (grapheme 数)
    pub ruby_len: usize,
    /// `shouldBeRemoved(data:)` の結果 (value が固定なので作るときに決める)
    pub removed: bool,
}

impl Elem {
    pub fn new(word: String, ruby: String, lcid: usize, rcid: usize, mid: usize, base: f32) -> Self {
        let word_len = count_graphemes(&word);
        let ruby_len = if ruby == word { word_len } else { count_graphemes(&ruby) };
        let mut e = Self { word, ruby, lcid, rcid, mid, base, adjust: 0.0, word_len, ruby_len, removed: false };
        e.removed = e.compute_removed();
        e
    }

    /// `DicdataElement.value()`: `min(0, base + adjust)`
    pub fn value(&self) -> f32 {
        let v = self.base + self.adjust;
        if v < 0.0 {
            v
        } else {
            0.0
        }
    }

    /// `DicdataStore.shouldBeRemoved(data:)`
    fn compute_removed(&self) -> bool {
        let d = self.value() - THRESHOLD;
        if d < 0.0 {
            return true;
        }
        let penalty = -2.0f32 / (self.word_len as f32);
        penalty < -d
    }

    /// BOS (始点)
    pub fn bos() -> Self {
        Self::new(String::new(), String::new(), CID_BOS, CID_BOS, MID_BOS_EOS, 0.0)
    }
}

/// 単語の種類 (`DicdataStore.wordTypes`): 3 = BOS/EOS, 0 = 前置, 1 = 内容, 2 = 後置
fn word_types() -> &'static [u8; 1320] {
    static TYPES: OnceLock<[u8; 1320]> = OnceLock::new();
    TYPES.get_or_init(|| {
        let mut t = [2u8; 1320];
        let in_range = |cid: usize, r: &[std::ops::Range<usize>]| r.iter().any(|x| x.contains(&cid));
        let inposition: [std::ops::Range<usize>; 6] = [561..868, 1283..1297, 1306..1310, 11..53, 555..557, 1281..1283];
        for (cid, slot) in t.iter_mut().enumerate() {
            *slot = if cid == CID_BOS || cid == CID_EOS {
                3
            } else if matches!(cid, 1315 | 6 | 557..=560) {
                0
            } else if in_range(cid, &inposition) || matches!(cid, 1314 | 3 | 2 | 4 | 5 | 1 | 9) {
                1
            } else {
                2
            };
        }
        t
    })
}

fn word_type(cid: usize) -> u8 {
    // 範囲外の ID は存在しないはずだが、パニックを避けて後置扱いにする
    word_types().get(cid).copied().unwrap_or(2)
}

/// `DicdataStore.isClause`: 文節の境界か
pub fn is_clause(former: usize, latter: usize) -> bool {
    let latter_type = word_type(latter);
    if latter_type == 3 {
        return false;
    }
    let former_type = word_type(former);
    if former_type == 3 {
        return false;
    }
    if latter_type == 0 || latter_type == 1 {
        return former_type != 0;
    }
    false
}

/// `DicdataStore.includeMMValueCalculation`
pub fn include_mm_value_calculation(e: &Elem) -> bool {
    // 非自立動詞
    if (895..=1280).contains(&e.lcid) || (895..=1280).contains(&e.rcid) {
        return true;
    }
    // 非自立名詞
    if (1297..=1305).contains(&e.lcid) || (1297..=1305).contains(&e.rcid) {
        return true;
    }
    // 内容語かどうか
    word_type(e.lcid) == 1 || word_type(e.rcid) == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_types_follow_cid_tables() {
        assert_eq!(word_type(0), 3);
        assert_eq!(word_type(1316), 3);
        assert_eq!(word_type(6), 0);
        assert_eq!(word_type(558), 0);
        assert_eq!(word_type(1315), 0);
        assert_eq!(word_type(11), 1);
        assert_eq!(word_type(52), 1);
        assert_eq!(word_type(53), 2);
        assert_eq!(word_type(1288), 1);
        assert_eq!(word_type(1297), 2);
        assert_eq!(word_type(1314), 1);
        assert_eq!(word_type(7), 2);
    }

    #[test]
    fn clause_boundaries_follow_word_types() {
        // 内容語 -> 後置機能語 は文節の切れ目でない
        assert!(!is_clause(1288, 200));
        // 内容語 -> 内容語 は切れ目
        assert!(is_clause(1288, 1288));
        // 前置 -> 内容語 は切れ目でない
        assert!(!is_clause(6, 1288));
        // 後置 -> 内容語 は切れ目
        assert!(is_clause(200, 1288));
        // BOS / EOS はどちらに来ても切れ目でない
        assert!(!is_clause(0, 1288));
        assert!(!is_clause(1288, 1316));
    }

    #[test]
    fn include_mm_for_content_and_dependent_words() {
        let mk = |l, r| Elem::new("あ".into(), "ア".into(), l, r, 1, -5.0);
        assert!(include_mm_value_calculation(&mk(1288, 1288)));
        assert!(include_mm_value_calculation(&mk(900, 200)));
        assert!(include_mm_value_calculation(&mk(200, 1300)));
        assert!(!include_mm_value_calculation(&mk(200, 200)));
    }

    #[test]
    fn removal_follows_threshold_and_length_penalty() {
        let mk = |w: &str, v| Elem::new(w.into(), w.into(), 1, 1, 1, v);
        // 値が -17 以下は除く
        assert!(mk("あ", -17.5).removed);
        // d = 3, penalty = -2/1 = -2: -2 < -3 は偽 -> 残る
        assert!(!mk("あ", -14.0).removed);
        // d = 1 (v = -16), 1 文字: -2 < -1 は真 -> 除く
        assert!(mk("あ", -16.0).removed);
        // 同じ値でも 3 文字なら penalty = -0.667 で残る
        assert!(!mk("あいう", -16.0).removed);
    }

    #[test]
    fn grapheme_count_handles_combining_marks() {
        assert_eq!(count_graphemes("かな"), 2);
        assert_eq!(count_graphemes("辻\u{E0100}"), 1);
        assert_eq!(count_graphemes("e\u{301}"), 1);
        assert_eq!(count_graphemes("a\r\nb"), 3);
        assert_eq!(count_graphemes("か\u{3099}"), 1);
    }

    #[test]
    fn canonical_key_unifies_compatibility_ideographs_and_combining_kana() {
        assert_eq!(canonical_key("\u{FA19}"), "\u{795E}");
        assert_eq!(canonical_key("か\u{3099}"), "が");
        assert_eq!(canonical_key("今日"), "今日");
    }
}
