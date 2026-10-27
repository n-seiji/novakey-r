//! 全文候補の派生候補。辞書の並びに無い、数詞+助数詞 (一等 → 一棟) と連濁した語 (がし → 貸し) を作る。
//! どちらも読みはそのままで表記だけを替え、順位は審判 (`judge`) に任せる。辞書は引数の `lookup` 越しに引くだけで、
//! ラティスや辞書の中身は触らない。
#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::rc::Rc;

use super::dict::Entry;
use super::elem::Elem;

/// 派生候補の元にする全文候補の数 (engine の順)
pub const ORIGINS: usize = 2;
/// 1 回の変換で足す派生候補の最大数
pub const MAX_VARIANTS: usize = 3;
/// 派生候補のスコアは元の候補から引く。審判が拮抗したときに元の候補を優先するための値。
pub const PENALTY: f32 = 6.0;
/// 連濁の語として 1 つの要素から作る数 (最良のスコアから)。最良が既に候補にあるときの次点のため。
const RENDAKU_PER_ELEM: usize = 2;
/// 連濁の語として探す名詞の品詞 ID (辞書の `lcid`)。内容語の名詞 1283..1297 のうち、固有名詞と数詞を除く。
const NOUN_LCIDS: std::ops::Range<usize> = 1283..1297;

/// 数詞: (漢数字, 算用数字, 読み)。読みは促音化したものも並べる。
const NUMERALS: &[(&str, &str, &[&str])] = &[
    ("一", "1", &["イチ", "イッ"]),
    ("二", "2", &["ニ"]),
    ("三", "3", &["サン"]),
    ("四", "4", &["ヨン", "ヨ"]),
    ("五", "5", &["ゴ"]),
    ("六", "6", &["ロク", "ロッ"]),
    ("七", "7", &["ナナ", "シチ"]),
    ("八", "8", &["ハチ", "ハッ"]),
    ("九", "9", &["キュウ"]),
    ("十", "10", &["ジュウ", "ジュッ", "ジッ"]),
    ("何", "何", &["ナン"]),
];

/// 助数詞: (表記, 読み)。同じ読みは先に並べた方を使う。
const COUNTERS: &[(&str, &[&str])] = &[
    ("棟", &["トウ"]),
    ("台", &["ダイ"]),
    ("枚", &["マイ"]),
    ("本", &["ホン", "ボン", "ポン"]),
    ("個", &["コ"]),
    ("件", &["ケン"]),
    ("回", &["カイ"]),
    ("階", &["カイ", "ガイ"]),
    ("冊", &["サツ"]),
    ("匹", &["ヒキ", "ビキ", "ピキ"]),
    ("杯", &["ハイ", "バイ", "パイ"]),
    ("軒", &["ケン", "ゲン"]),
    ("人", &["ニン"]),
    ("名", &["メイ"]),
    ("点", &["テン"]),
    ("着", &["チャク"]),
    ("泊", &["ハク", "パク"]),
];

/// 派生候補。`data` は学習にそのまま使う。
#[derive(Debug, Clone)]
pub struct Variant {
    pub text: String,
    pub data: Vec<Rc<Elem>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DigitStyle {
    Ascii,
    Wide,
    Kanji,
}

fn is_kana(c: char) -> bool {
    matches!(c as u32, 0x3041..=0x3096 | 0x30A1..=0x30FA | 0x30FC)
}

fn is_hiragana(c: char) -> bool {
    matches!(c as u32, 0x3041..=0x3096 | 0x30FC)
}

fn is_kanji(c: char) -> bool {
    matches!(c as u32, 0x4E00..=0x9FFF | 0x3005)
}

/// 数詞として読み替えてよい表記か。数字・漢数字・何で始まるか、全部かなのときだけ (発見 → 八件 を防ぐ)。
fn digit_style(surface: &str) -> Option<DigitStyle> {
    let first = surface.chars().next()?;
    match first {
        '0'..='9' => Some(DigitStyle::Ascii),
        '０'..='９' => Some(DigitStyle::Wide),
        '一' | '二' | '三' | '四' | '五' | '六' | '七' | '八' | '九' | '十' | '百' | '千' | '何' => Some(DigitStyle::Kanji),
        _ if surface.chars().all(is_kana) => Some(DigitStyle::Kanji),
        _ => None,
    }
}

fn numeral_text(kanji: &str, digits: &str, style: DigitStyle) -> String {
    match style {
        DigitStyle::Kanji => kanji.to_string(),
        DigitStyle::Ascii => digits.to_string(),
        DigitStyle::Wide => digits.chars().map(|c| c.to_digit(10).and_then(|d| char::from_u32('０' as u32 + d)).unwrap_or(c)).collect(),
    }
}

/// 数詞+助数詞に読み替えた要素 (B)。読みの切り方ごとに 1 つ。表記が元と同じものは除く。
fn counter_variants(e: &Elem) -> Vec<Elem> {
    let Some(style) = digit_style(&e.word) else {
        return Vec::new();
    };
    let mut out: Vec<Elem> = Vec::new();
    for &(kanji, digits, readings) in NUMERALS {
        let numeral = numeral_text(kanji, digits, if kanji == "何" { DigitStyle::Kanji } else { style });
        for reading in readings {
            let Some(rest) = e.ruby.strip_prefix(reading).filter(|r| !r.is_empty()) else {
                continue;
            };
            let Some(&(counter, _)) = COUNTERS.iter().find(|(_, rs)| rs.contains(&rest)) else {
                continue;
            };
            let word = format!("{numeral}{counter}");
            if word != e.word && !out.iter().any(|v| v.word == word) {
                out.push(Elem::new(word, e.ruby.clone(), e.lcid, e.rcid, e.mid, e.value()));
            }
        }
    }
    out
}

fn is_voiced(c: char) -> bool {
    "ガギグゲゴザジズゼゾダヂヅデドバビブベボパピプペポ".contains(c)
}

/// 連濁した語に読み替えた要素 (F)。前の語が漢字を含み、`e` が濁音始まりのひらがな 2 字以上のとき、
/// 同じ読みの名詞 (表記に漢字を含む) のうち辞書のスコアが良い順に `RENDAKU_PER_ELEM` 個 (貸し/ガシ)。
fn rendaku_variants(prev: &Elem, e: &Elem, lookup: &dyn Fn(&str) -> Vec<Entry>) -> Vec<Elem> {
    if e.ruby.chars().count() < 2 || !e.ruby.chars().next().is_some_and(is_voiced) {
        return Vec::new();
    }
    if !e.word.chars().all(is_hiragana) || !prev.word.chars().any(is_kanji) {
        return Vec::new();
    }
    let mut found: Vec<Entry> = lookup(&e.ruby)
        .into_iter()
        .filter(|h| h.ruby == e.ruby && NOUN_LCIDS.contains(&(h.lcid as usize)) && h.word.chars().any(is_kanji))
        .collect();
    found.sort_by(|a, b| b.value.total_cmp(&a.value));
    let mut out: Vec<Elem> = Vec::new();
    for h in found {
        if out.len() < RENDAKU_PER_ELEM && !out.iter().any(|o| o.word == h.word) {
            out.push(Elem::new(h.word, h.ruby, h.lcid as usize, h.rcid as usize, h.mid as usize, h.value));
        }
    }
    out
}

fn with_replaced(data: &[Rc<Elem>], i: usize, e: Elem) -> Vec<Rc<Elem>> {
    let mut v = data.to_vec();
    v[i] = Rc::new(e);
    v
}

fn counter_replacements(data: &[Rc<Elem>]) -> Vec<Vec<Rc<Elem>>> {
    (0..data.len()).flat_map(|i| counter_variants(&data[i]).into_iter().map(move |e| (i, e))).map(|(i, e)| with_replaced(data, i, e)).collect()
}

fn rendaku_replacements(data: &[Rc<Elem>], lookup: &dyn Fn(&str) -> Vec<Entry>) -> Vec<Vec<Rc<Elem>>> {
    (1..data.len())
        .flat_map(|i| rendaku_variants(&data[i - 1], &data[i], lookup).into_iter().map(move |e| (i, e)))
        .map(|(i, e)| with_replaced(data, i, e))
        .collect()
}

fn surface(data: &[Rc<Elem>]) -> String {
    data.iter().map(|e| e.word.as_str()).collect::<String>().replace('\u{1F}', "")
}

/// `origins` (元の全文候補の語列。engine の順) から派生候補を作る。元ごとに B+F、B、F の順で、
/// `exclude` (既にある候補の文字列) と重ならないものを `MAX_VARIANTS` まで。
pub fn generate(origins: &[&[Rc<Elem>]], exclude: &HashSet<String>, lookup: &dyn Fn(&str) -> Vec<Entry>) -> Vec<(usize, Variant)> {
    let mut seen = exclude.clone();
    let mut out = Vec::new();
    for (k, data) in origins.iter().enumerate() {
        let counters = counter_replacements(data);
        let both = counters.iter().flat_map(|d| rendaku_replacements(d, lookup));
        let rendaku = rendaku_replacements(data, lookup);
        for d in both.chain(counters.iter().cloned()).chain(rendaku) {
            let text = surface(&d);
            if out.len() < MAX_VARIANTS && seen.insert(text.clone()) {
                out.push((k, Variant { text, data: d }));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn elem(word: &str, ruby: &str, lcid: usize) -> Rc<Elem> {
        Rc::new(Elem::new(word.into(), ruby.into(), lcid, lcid, 1, -8.0))
    }

    fn no_dict(_: &str) -> Vec<Entry> {
        Vec::new()
    }

    fn texts(v: &[(usize, Variant)]) -> Vec<&str> {
        v.iter().map(|(_, v)| v.text.as_str()).collect()
    }

    fn gashi(ruby: &str) -> Vec<Entry> {
        if ruby != "ガシ" {
            return Vec::new();
        }
        let e = |word: &str, lcid, value| Entry { ruby: "ガシ".into(), word: word.into(), lcid, rcid: lcid, mid: 1, value };
        vec![e("貸し", 1285, -14.2), e("貸し", 735, -14.2), e("我師", 1300, -9.0), e("餓死", 1283, -11.6), e("賀詞", 1285, -15.8)]
    }

    #[test]
    fn 一等の読みから一棟ができる() {
        let data = [elem("一等", "イットウ", 1285)];
        let v = generate(&[&data], &HashSet::new(), &no_dict);
        assert_eq!(texts(&v), ["一棟"]);
        assert_eq!(v[0].1.data[0].ruby, "イットウ");
        assert_eq!(v[0].1.data[0].lcid, 1285);
    }

    #[test]
    fn 数字で始まる表記は同じ字種の数字で返す() {
        assert_eq!(texts(&generate(&[&[elem("三代", "サンダイ", 1285)]], &HashSet::new(), &no_dict)), ["三台"]);
        assert_eq!(texts(&generate(&[&[elem("3代", "サンダイ", 1285)]], &HashSet::new(), &no_dict)), ["3台"]);
        assert_eq!(texts(&generate(&[&[elem("３代", "サンダイ", 1285)]], &HashSet::new(), &no_dict)), ["３台"]);
        assert_eq!(texts(&generate(&[&[elem("10代", "ジュウダイ", 1285)]], &HashSet::new(), &no_dict)), ["10台"]);
    }

    #[test]
    fn かなだけの表記も数詞と助数詞に読み替える() {
        let v = generate(&[&[elem("いちまい", "イチマイ", 1285)]], &HashSet::new(), &no_dict);
        assert_eq!(texts(&v), ["一枚"]);
        let v = generate(&[&[elem("いっぽん", "イッポン", 1285)]], &HashSet::new(), &no_dict);
        assert_eq!(texts(&v), ["一本"]);
    }

    #[test]
    fn 数字で始まらない表記は読み替えない() {
        for (w, r) in [("発見", "ハッケン"), ("日本", "ニホン"), ("誤解", "ゴカイ"), ("会社", "カイシャ")] {
            assert!(generate(&[&[elem(w, r, 1285)]], &HashSet::new(), &no_dict).is_empty(), "{w}");
        }
    }

    #[test]
    fn 表記が変わらない読み替えは作らない() {
        assert!(generate(&[&[elem("三台", "サンダイ", 1285)]], &HashSet::new(), &no_dict).is_empty());
    }

    #[test]
    fn 同じ読みの助数詞は表の先頭だけを使う() {
        let v = generate(&[&[elem("一頭", "イットウ", 1285)]], &HashSet::new(), &no_dict);
        assert_eq!(texts(&v), ["一棟"]);
        let v = generate(&[&[elem("一戸", "イッコ", 1285)]], &HashSet::new(), &no_dict);
        assert_eq!(texts(&v), ["一個"]);
    }

    #[test]
    fn 漢字の語に続く濁音のひらがなは同じ読みの名詞に直す() {
        let data = [elem("一棟", "イットウ", 1285), elem("がし", "ガシ", 1285)];
        let v = generate(&[&data], &HashSet::new(), &gashi);
        assert_eq!(texts(&v), ["一棟餓死", "一棟貸し"]);
        assert_eq!(v[1].1.data[1].ruby, "ガシ");
        assert_eq!(v[1].1.data[1].lcid, 1285);
    }

    #[test]
    fn 連濁は名詞の中からスコアの良い順に2つまで漢字を含む語を選ぶ() {
        let data = [elem("車", "クルマ", 1285), elem("がし", "ガシ", 1285)];
        assert_eq!(texts(&generate(&[&data], &HashSet::new(), &gashi)), ["車餓死", "車貸し"]);
    }

    #[test]
    fn 最良の語が既に候補にあれば次点の貸しが残る() {
        let data = [elem("一棟", "イットウ", 1285), elem("がし", "ガシ", 1285)];
        let exclude: HashSet<String> = ["一棟餓死".to_string()].into();
        assert_eq!(texts(&generate(&[&data], &exclude, &gashi)), ["一棟貸し"]);
    }

    #[test]
    fn 連濁は前がひらがなの語や濁音でない語では起きない() {
        assert!(generate(&[&[elem("の", "ノ", 200), elem("がし", "ガシ", 1285)]], &HashSet::new(), &gashi).is_empty());
        assert!(generate(&[&[elem("会社", "カイシャ", 1285), elem("かし", "カシ", 1285)]], &HashSet::new(), &gashi).is_empty());
        assert!(generate(&[&[elem("会社", "カイシャ", 1285), elem("が", "ガ", 200)]], &HashSet::new(), &gashi).is_empty());
        assert!(generate(&[&[elem("がし", "ガシ", 1285)]], &HashSet::new(), &gashi).is_empty());
    }

    #[test]
    fn 重複と既存の候補は除く() {
        let data = [elem("一等", "イットウ", 1285)];
        let exclude: HashSet<String> = ["一棟".to_string()].into();
        assert!(generate(&[&data, &data], &exclude, &no_dict).is_empty());
        assert_eq!(generate(&[&data, &data], &HashSet::new(), &no_dict).len(), 1);
    }

    #[test]
    fn 上限を超えない上で一棟貸しは残る() {
        let data = [elem("別荘", "ベッソウ", 1285), elem("の", "ノ", 200), elem("一等", "イットウ", 1285), elem("がし", "ガシ", 1285)];
        let v = generate(&[&data], &HashSet::new(), &gashi);
        assert!(texts(&v).contains(&"別荘の一棟貸し"), "{:?}", texts(&v));
        assert!(v.len() <= MAX_VARIANTS);
        let many: Vec<Rc<Elem>> = (0..6).map(|_| elem("一等", "イットウ", 1285)).collect();
        let v = generate(&[&many], &HashSet::new(), &no_dict);
        assert_eq!(v.len(), MAX_VARIANTS);
    }

    #[test]
    fn 先の元の候補の派生が先に並ぶ() {
        let a = [elem("一等", "イットウ", 1285)];
        let b = [elem("三代", "サンダイ", 1285)];
        let v = generate(&[&a, &b], &HashSet::new(), &no_dict);
        assert_eq!(texts(&v), ["一棟", "三台"]);
        assert_eq!(v.iter().map(|(k, _)| *k).collect::<Vec<_>>(), [0, 1]);
    }
}
