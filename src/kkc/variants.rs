//! 全文候補の派生候補。辞書の並びに無い、数詞+助数詞 (一等 → 一棟) と連濁した語 (がし → 貸し) を作る。
//! どちらも読みはそのままで表記だけを替え、順位は審判 (`judge`) に任せる。辞書は引数の `lookup` 越しに引くだけで、
//! ラティスや辞書の中身は触らない。
#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::rc::Rc;

use super::dict::Entry;
use super::elem::{Elem, CID_NUMBER, CID_PROPER_NOUN};

/// 派生候補の元にする全文候補の数 (engine の順)
pub const ORIGINS: usize = 4;
/// 1 回の変換で足す派生候補の最大数
pub const MAX_VARIANTS: usize = 3;
/// 派生候補のスコアは元の候補から引く。審判が拮抗したときに元の候補を優先するための値。
pub const PENALTY: f32 = 6.0;
/// 連濁の語として 1 つの要素から作る数 (最良のスコアから)。最良が既に候補にあるときの次点のため。
const RENDAKU_PER_ELEM: usize = 2;
/// 連濁の語として探す名詞の品詞 ID (辞書の `lcid`)。内容語の名詞 1283..1297 のうち、固有名詞 (`CID_PROPER_NOUN` 1288 から
/// 地名・人名などの 1294 まで) と数詞 (`CID_NUMBER` 1295) を除く。
const NOUN_LCIDS: std::ops::Range<usize> = 1283..1297;
/// `NOUN_LCIDS` の中の、固有名詞と数詞の ID (1288..=1295)。連濁の語には使わない。
const EXCLUDED_LCIDS: std::ops::RangeInclusive<usize> = CID_PROPER_NOUN..=CID_NUMBER;
/// 一般名詞の品詞 ID。連濁の語はこれを先に並べる (餓死より貸し)。
const LCID_GENERAL_NOUN: usize = 1285;

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

/// 数詞として読み替えてよい表記なら、作る数字の字種 (先頭が同じ字種、次が別の字種)。数字・漢数字・何で始まるか、
/// 全部かなのときだけ (発見 → 八件 を防ぐ)。算用数字 ↔ 漢数字は両方作る (全角は算用数字も)。
/// 審判が選んだ別の字種の候補 (車を3代 → 車を3台) の元が、漢数字の候補しかなくても出るように。
fn digit_styles(surface: &str) -> Vec<DigitStyle> {
    let Some(first) = surface.chars().next() else {
        return Vec::new();
    };
    match first {
        '0'..='9' => vec![DigitStyle::Ascii, DigitStyle::Kanji],
        '０'..='９' => vec![DigitStyle::Wide, DigitStyle::Ascii],
        '何' => vec![DigitStyle::Kanji],
        '一' | '二' | '三' | '四' | '五' | '六' | '七' | '八' | '九' | '十' | '百' | '千' => vec![DigitStyle::Kanji, DigitStyle::Ascii],
        _ if surface.chars().all(is_kana) => vec![DigitStyle::Kanji],
        _ => Vec::new(),
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
    let styles = digit_styles(&e.word);
    let mut out: Vec<Elem> = Vec::new();
    for &(kanji, digits, readings) in NUMERALS {
        for reading in readings {
            let Some(rest) = e.ruby.strip_prefix(reading).filter(|r| !r.is_empty()) else {
                continue;
            };
            let Some(&(counter, _)) = COUNTERS.iter().find(|(_, rs)| rs.contains(&rest)) else {
                continue;
            };
            for &style in &styles {
                let numeral = numeral_text(kanji, digits, if kanji == "何" { DigitStyle::Kanji } else { style });
                let word = format!("{numeral}{counter}");
                if word != e.word && !out.iter().any(|v| v.word == word) {
                    out.push(Elem::new(word, e.ruby.clone(), e.lcid, e.rcid, e.mid, e.value()));
                }
            }
        }
    }
    out
}

/// 漢数字の要素 (第|八|世代 の 八、第十 の 第十) を算用数字に読み替えた要素 (N)。第のつく要素 (第十) は 1 つでも、
/// 数詞だけの要素 (品詞 1295) は他の要素と一緒の候補のときだけ (八 → 8 を単独では出さない)。読みが表の数詞と合うものだけ。
fn arabic_variant(e: &Elem, alone: bool) -> Option<Elem> {
    let (prefix, ruby_prefix, word, ruby) = match (e.word.strip_prefix('第'), e.ruby.strip_prefix("ダイ")) {
        (Some(w), Some(r)) => ("第", "ダイ", w, r),
        _ if e.lcid == CID_NUMBER && !alone => ("", "", e.word.as_str(), e.ruby.as_str()),
        _ => return None,
    };
    let &(_, digits, _) = NUMERALS.iter().find(|&&(kanji, digits, readings)| kanji == word && kanji != digits && readings.contains(&ruby))?;
    Some(Elem::new(format!("{prefix}{digits}"), format!("{ruby_prefix}{ruby}"), e.lcid, e.rcid, e.mid, e.value()))
}

fn is_voiced(c: char) -> bool {
    "ガギグゲゴザジズゼゾダヂヅデドバビブベボパピプペポ".contains(c)
}

/// 連濁した語に読み替えた要素 (F)。前の語が漢字を含み、`e` が濁音始まりのひらがな 2 字以上のとき、
/// 同じ読みの名詞 (表記に漢字を含む) のうち、一般名詞、辞書のスコアの順に `RENDAKU_PER_ELEM` 個 (貸し/ガシ)。
fn rendaku_variants(prev: &Elem, e: &Elem, lookup: &dyn Fn(&str) -> Vec<Entry>) -> Vec<Elem> {
    if e.ruby.chars().count() < 2 || !e.ruby.chars().next().is_some_and(is_voiced) {
        return Vec::new();
    }
    if !e.word.chars().all(is_hiragana) || !prev.word.chars().any(is_kanji) {
        return Vec::new();
    }
    let mut found: Vec<Entry> = lookup(&e.ruby)
        .into_iter()
        .filter(|h| h.ruby == e.ruby && NOUN_LCIDS.contains(&(h.lcid as usize)) && !EXCLUDED_LCIDS.contains(&(h.lcid as usize)) && h.word.chars().any(is_kanji))
        .collect();
    // 一般名詞 (1285) を先に、同じなら辞書のスコアが良い順
    found.sort_by(|a, b| {
        let general = |h: &Entry| h.lcid as usize == LCID_GENERAL_NOUN;
        general(b).cmp(&general(a)).then(b.value.total_cmp(&a.value))
    });
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

fn arabic_replacements(data: &[Rc<Elem>]) -> Vec<Vec<Rc<Elem>>> {
    (0..data.len()).filter_map(|i| arabic_variant(&data[i], data.len() < 2).map(|e| with_replaced(data, i, e))).collect()
}

fn rendaku_replacements(data: &[Rc<Elem>], lookup: &dyn Fn(&str) -> Vec<Entry>) -> Vec<Vec<Rc<Elem>>> {
    (1..data.len())
        .flat_map(|i| rendaku_variants(&data[i - 1], &data[i], lookup).into_iter().map(move |e| (i, e)))
        .map(|(i, e)| with_replaced(data, i, e))
        .collect()
}

/// 〜なそう の後ろに付けてよい短い続き: (読み, 表記)。
const NASOU_TAILS: &[(&str, &str)] = &[("ダッタ", "だった"), ("デス", "です"), ("ダ", "だ"), ("ナ", "な"), ("ニ", "に"), ("", "")];
/// 〜なそう の派生候補として足す最大数 (`MAX_VARIANTS` とは別枠。他の派生に押し出されないように)
pub const NASOU_MAX: usize = 2;
/// 動詞の未然形などの要素 (起き・食べ・行か・知ら) の `rcid` の範囲。ない の前がこの範囲のときだけ、ない を なそう に替える
/// (歯ない・内 など名詞や助詞に続く ない を動詞の否定と取り違えないため)。
const VERB_RCIDS: std::ops::Range<usize> = 570..1000;
/// 形容詞 1 語 (儚い・危ない・少ない) の `rcid`
const ADJECTIVE_RCID: usize = 19;
/// 否定まで 1 語の動詞 (わからない・分からない) の `rcid`
const NEGATIVE_WORD_RCID: usize = 473;

/// 読み (カタカナ) が `〜ナソウ` + 短い続き で終わるとき、`ナソウ` を `ナイ` に替えた続きなしの読みと、その続き。
pub fn nasou_split(katakana: &str) -> Option<(String, (&'static str, &'static str))> {
    NASOU_TAILS.iter().find_map(|&tail| {
        let stem = katakana.strip_suffix(tail.0)?.strip_suffix("ナソウ").filter(|s| !s.is_empty())?;
        Some((format!("{stem}ナイ"), tail))
    })
}

/// `negative` (〜ナイ の変換結果の全文候補。(スコア, 語列)) のうち 動詞+ない で終わるものの ない を なそう に替え、`tail` を付けた派生候補。
/// スコアは元の候補のスコアに続きの辞書のスコアを足したもの。
pub fn nasou_variants(negative: &[(f32, &[Rc<Elem>])], tail: (&str, &str), exclude: &HashSet<String>, max: usize, lookup: &dyn Fn(&str) -> Vec<Entry>) -> Vec<(f32, Variant)> {
    // はかない には 儚い (形容詞 1 語) の候補があるので、吐かない (動詞) 由来の 吐かなそう を作らない。形容詞 + そう は 動詞の否定の ない を替えた形ではないため。
    // わからない も同じ: 1 語の候補を差し替えられないので、次点の 解ら + ない から 解らなそう だけを作ってしまう。
    if negative.iter().any(|(_, d)| d.len() == 1 && matches!(d[0].rcid, ADJECTIVE_RCID | NEGATIVE_WORD_RCID) && d[0].ruby.ends_with("ナイ")) {
        return Vec::new();
    }
    let tail_elem = if tail.0.is_empty() {
        None
    } else {
        let best = lookup(tail.0).into_iter().filter(|h| h.ruby == tail.0 && h.word == tail.1).max_by(|a, b| a.value.total_cmp(&b.value));
        match best {
            Some(h) => Some(Elem::new(h.word, h.ruby, h.lcid as usize, h.rcid as usize, h.mid as usize, h.value)),
            None => return Vec::new(),
        }
    };
    let mut seen = exclude.clone();
    let mut out = Vec::new();
    for &(score, data) in negative {
        let [.., prev, last] = data else {
            continue;
        };
        if last.word != "ない" || last.ruby != "ナイ" || !VERB_RCIDS.contains(&prev.rcid) {
            continue;
        }
        let mut d = data[..data.len() - 1].to_vec();
        d.push(Rc::new(Elem::new("なそう".into(), "ナソウ".into(), last.lcid, last.rcid, last.mid, last.base)));
        let mut score = score;
        if let Some(t) = &tail_elem {
            score += t.value();
            d.push(Rc::new(t.clone()));
        }
        let text = surface(&d);
        // 全部ひらがな (おきなそう) は読みそのものなので足さない。審判が好んで漢字の候補を押しのけるため。
        if out.len() < max && text.chars().any(|c| !is_hiragana(c)) && seen.insert(text.clone()) {
            out.push((score, Variant { text, data: d }));
        }
    }
    out
}

fn surface(data: &[Rc<Elem>]) -> String {
    data.iter().map(|e| e.word.as_str()).collect::<String>().replace('\u{1F}', "")
}

/// `origins` (元の全文候補の語列。engine の順) から派生候補を作る。元ごとに B+F、B、F、N の順で、
/// `exclude` (既にある候補の文字列) と重ならないものを `MAX_VARIANTS` まで。
pub fn generate(origins: &[&[Rc<Elem>]], exclude: &HashSet<String>, lookup: &dyn Fn(&str) -> Vec<Entry>) -> Vec<(usize, Variant)> {
    let mut seen = exclude.clone();
    let mut out = Vec::new();
    for (k, data) in origins.iter().enumerate() {
        let counters = counter_replacements(data);
        let both = counters.iter().flat_map(|d| rendaku_replacements(d, lookup));
        let rendaku = rendaku_replacements(data, lookup);
        for d in both.chain(counters.iter().cloned()).chain(rendaku).chain(arabic_replacements(data)) {
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
    fn 単独の漢数字は算用数字にも読み替える() {
        let dai = elem("第", "ダイ", 558);
        let sedai = elem("世代", "セダイ", 1285);
        let v = generate(&[&[dai.clone(), elem("八", "ハチ", CID_NUMBER), sedai.clone()]], &HashSet::new(), &no_dict);
        assert_eq!(texts(&v), ["第8世代"]);
        assert_eq!(v[0].1.data[1].ruby, "ハチ");
        assert_eq!(v[0].1.data[1].lcid, CID_NUMBER);
        assert_eq!(texts(&generate(&[&[dai, elem("十", "ジュウ", CID_NUMBER), sedai.clone()]], &HashSet::new(), &no_dict)), ["第10世代"]);
        assert_eq!(texts(&generate(&[&[elem("八", "ハチ", CID_NUMBER), sedai]], &HashSet::new(), &no_dict)), ["8世代"]);
    }

    #[test]
    fn 第のつく漢数字の要素も算用数字にする() {
        let sedai = elem("世代", "セダイ", 1285);
        let v = generate(&[&[elem("第十", "ダイジュウ", 1290), sedai]], &HashSet::new(), &no_dict);
        assert_eq!(texts(&v), ["第10世代"]);
        assert_eq!(v[0].1.data[0].ruby, "ダイジュウ");
        assert_eq!(texts(&generate(&[&[elem("第九", "ダイキュウ", 1290)]], &HashSet::new(), &no_dict)), ["第9"]);
        assert!(generate(&[&[elem("第一印象", "ダイイチインショウ", 1285)]], &HashSet::new(), &no_dict).is_empty());
    }

    #[test]
    fn 数詞でない要素や1要素だけの候補は算用数字にしない() {
        assert!(generate(&[&[elem("八", "ハチ", CID_NUMBER)]], &HashSet::new(), &no_dict).is_empty());
        assert!(generate(&[&[elem("鉢", "ハチ", 1285), elem("世代", "セダイ", 1285)]], &HashSet::new(), &no_dict).is_empty());
        assert!(generate(&[&[elem("八", "ヤ", CID_NUMBER), elem("世代", "セダイ", 1285)]], &HashSet::new(), &no_dict).is_empty());
    }

    #[test]
    fn 一等の読みから一棟ができる() {
        let data = [elem("一等", "イットウ", 1285)];
        let v = generate(&[&data], &HashSet::new(), &no_dict);
        assert_eq!(texts(&v), ["一棟", "1棟"]);
        assert_eq!(v[0].1.data[0].ruby, "イットウ");
        assert_eq!(v[0].1.data[0].lcid, 1285);
    }

    #[test]
    fn 数字で始まる表記は同じ字種の数字で返す() {
        assert_eq!(texts(&generate(&[&[elem("三代", "サンダイ", 1285)]], &HashSet::new(), &no_dict)), ["三台", "3台"]);
        assert_eq!(texts(&generate(&[&[elem("3代", "サンダイ", 1285)]], &HashSet::new(), &no_dict)), ["3台", "三台"]);
        assert_eq!(texts(&generate(&[&[elem("３代", "サンダイ", 1285)]], &HashSet::new(), &no_dict)), ["３台", "3台"]);
        assert_eq!(texts(&generate(&[&[elem("10代", "ジュウダイ", 1285)]], &HashSet::new(), &no_dict)), ["10台", "十台"]);
    }

    #[test]
    fn 漢数字の元からも算用数字の助数詞ができる_何とかなは字種を替えない() {
        let v = generate(&[&[elem("車", "クルマ", 1285), elem("三代", "サンダイ", 1285)]], &HashSet::new(), &no_dict);
        assert_eq!(texts(&v), ["車三台", "車3台"]);
        assert_eq!(texts(&generate(&[&[elem("何代", "ナンダイ", 1285)]], &HashSet::new(), &no_dict)), ["何台"]);
        assert_eq!(texts(&generate(&[&[elem("いちまい", "イチマイ", 1285)]], &HashSet::new(), &no_dict)), ["一枚"]);
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
        assert_eq!(texts(&generate(&[&[elem("三台", "サンダイ", 1285)]], &HashSet::new(), &no_dict)), ["3台"]);
        assert_eq!(texts(&generate(&[&[elem("3台", "サンダイ", 1285)]], &HashSet::new(), &no_dict)), ["三台"]);
    }

    #[test]
    fn 同じ読みの助数詞は表の先頭だけを使う() {
        let v = generate(&[&[elem("一頭", "イットウ", 1285)]], &HashSet::new(), &no_dict);
        assert_eq!(texts(&v), ["一棟", "1棟"]);
        let v = generate(&[&[elem("一戸", "イッコ", 1285)]], &HashSet::new(), &no_dict);
        assert_eq!(texts(&v), ["一個", "1個"]);
    }

    #[test]
    fn 漢字の語に続く濁音のひらがなは同じ読みの名詞に直す() {
        let data = [elem("一等", "イットウ", 1285), elem("がし", "ガシ", 1285)];
        let v = generate(&[&data], &HashSet::new(), &gashi);
        assert_eq!(texts(&v), ["一棟貸し", "一棟賀詞", "1棟貸し"]);
        assert_eq!(v[0].1.data[1].ruby, "ガシ");
        assert_eq!(v[0].1.data[1].lcid, 1285);
    }

    #[test]
    fn 連濁は名詞の中からスコアの良い順に2つまで漢字を含む語を選ぶ() {
        let data = [elem("車", "クルマ", 1285), elem("がし", "ガシ", 1285)];
        assert_eq!(texts(&generate(&[&data], &HashSet::new(), &gashi)), ["車貸し", "車賀詞"]);
    }

    #[test]
    fn 連濁は一般名詞を先に_固有名詞と数詞は使わない() {
        let entries = |ruby: &str| -> Vec<Entry> {
            if ruby != "ガシ" {
                return Vec::new();
            }
            let e = |word: &str, lcid, value| Entry { ruby: "ガシ".into(), word: word.into(), lcid, rcid: lcid, mid: 1, value };
            vec![e("餓死", 1283, -11.6), e("賀志", 1288, -9.0), e("画紙", 1295, -9.5), e("貸し", 1285, -14.2), e("賀詞", 1285, -15.8)]
        };
        let data = [elem("車", "クルマ", 1285), elem("がし", "ガシ", 1285)];
        assert_eq!(texts(&generate(&[&data], &HashSet::new(), &entries)), ["車貸し", "車賀詞"]);
    }

    #[test]
    fn 最良の語が既に候補にあれば次点の貸しが残る() {
        let data = [elem("一等", "イットウ", 1285), elem("がし", "ガシ", 1285)];
        let exclude: HashSet<String> = ["一棟貸し".to_string()].into();
        assert_eq!(texts(&generate(&[&data], &exclude, &gashi)), ["一棟賀詞", "1棟貸し", "1棟賀詞"]);
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
        let exclude: HashSet<String> = ["一棟".to_string(), "1棟".to_string()].into();
        assert!(generate(&[&data, &data], &exclude, &no_dict).is_empty());
        assert_eq!(generate(&[&data, &data], &HashSet::new(), &no_dict).len(), 2);
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
        assert_eq!(texts(&v), ["一棟", "1棟", "三台"]);
        assert_eq!(v.iter().map(|(k, _)| *k).collect::<Vec<_>>(), [0, 0, 1]);
    }

    fn verb_nai(stem: &str, ruby: &str) -> Vec<Rc<Elem>> {
        vec![Rc::new(Elem::new(stem.into(), ruby.into(), 622, 622, 43, -6.0)), Rc::new(Elem::new("ない".into(), "ナイ".into(), 473, 473, 468, -1.9))]
    }

    fn tail_dict(ruby: &str) -> Vec<Entry> {
        let e = |word: &str| Entry { ruby: ruby.into(), word: word.into(), lcid: 400, rcid: 400, mid: 1, value: -2.0 };
        match ruby {
            "ダ" => vec![e("だ"), e("駄")],
            "デス" => vec![e("です")],
            _ => Vec::new(),
        }
    }

    #[test]
    fn なそうは_なそうと続きで終わる読みだけを切り出す() {
        assert_eq!(nasou_split("オキナソウ"), Some(("オキナイ".into(), ("", ""))));
        assert_eq!(nasou_split("オキナソウダ"), Some(("オキナイ".into(), ("ダ", "だ"))));
        assert_eq!(nasou_split("オキナソウデス"), Some(("オキナイ".into(), ("デス", "です"))));
        assert_eq!(nasou_split("オキナソウダッタ"), Some(("オキナイ".into(), ("ダッタ", "だった"))));
        assert_eq!(nasou_split("ナソウ"), None);
        assert_eq!(nasou_split("ハナソウ").map(|(n, _)| n), Some("ハナイ".into()));
        assert_eq!(nasou_split("オキナサソウ"), None);
        assert_eq!(nasou_split("オキナイ"), None);
    }

    #[test]
    fn 起きないの_ないを_なそうに替えて読みも揃える() {
        let neg = verb_nai("起き", "オキ");
        let v = nasou_variants(&[(-16.6, &neg)], ("", ""), &HashSet::new(), NASOU_MAX, &no_dict);
        assert_eq!(texts(&v.iter().map(|(_, v)| (0, v.clone())).collect::<Vec<_>>()), ["起きなそう"]);
        let ruby: String = v[0].1.data.iter().map(|e| e.ruby.as_str()).collect();
        assert_eq!(ruby, "オキナソウ");
        assert_eq!(v[0].0, -16.6);
    }

    #[test]
    fn 続きは辞書の語を付け_スコアに足す() {
        let neg = verb_nai("起き", "オキ");
        let v = nasou_variants(&[(-16.0, &neg)], ("ダ", "だ"), &HashSet::new(), NASOU_MAX, &tail_dict);
        assert_eq!(v[0].1.text, "起きなそうだ");
        assert_eq!(v[0].1.data.iter().map(|e| e.ruby.as_str()).collect::<String>(), "オキナソウダ");
        assert_eq!(v[0].0, -18.0);
        assert!(nasou_variants(&[(-16.0, &neg)], ("ダッタ", "だった"), &HashSet::new(), NASOU_MAX, &tail_dict).is_empty(), "続きが辞書になければ作らない");
    }

    #[test]
    fn 全部ひらがなの候補は作らない() {
        let neg = verb_nai("おき", "オキ");
        assert!(nasou_variants(&[(-1.0, &neg)], ("", ""), &HashSet::new(), NASOU_MAX, &no_dict).is_empty());
    }

    #[test]
    fn 動詞でない語に続く_ないや内は_なそうにしない() {
        let particle = vec![Rc::new(Elem::new("は".into(), "ハ".into(), 261, 261, 468, -1.4)), Rc::new(Elem::new("ない".into(), "ナイ".into(), 473, 473, 468, -1.9))];
        let noun = vec![Rc::new(Elem::new("お".into(), "オ".into(), 560, 560, 437, -3.5)), Rc::new(Elem::new("内".into(), "ナイ".into(), 1285, 1285, 397, -7.1))];
        let single = vec![Rc::new(Elem::new("ない".into(), "ナイ".into(), 473, 473, 468, -1.9))];
        assert!(nasou_variants(&[(-1.0, &particle), (-1.0, &noun), (-1.0, &single)], ("", ""), &HashSet::new(), NASOU_MAX, &no_dict).is_empty());
    }

    #[test]
    fn なそうの派生は既存の候補と重ならず最大数を超えない() {
        let a = verb_nai("起き", "オキ");
        let b = verb_nai("置き", "オキ");
        let c = verb_nai("沖", "オキ");
        let all: Vec<(f32, &[Rc<Elem>])> = vec![(-1.0, &a), (-2.0, &b), (-3.0, &c)];
        assert_eq!(nasou_variants(&all, ("", ""), &HashSet::new(), NASOU_MAX, &no_dict).len(), NASOU_MAX);
        let exclude: HashSet<String> = ["起きなそう".to_string()].into();
        assert_eq!(nasou_variants(&all, ("", ""), &exclude, NASOU_MAX, &no_dict)[0].1.text, "置きなそう");
    }

    #[test]
    fn 形容詞が1語で出る読みは_動詞の否定が混じっても_なそうにしない() {
        let adj = vec![Rc::new(Elem::new("儚い".into(), "ハカナイ".into(), 19, 19, 468, -7.0))];
        let verb = verb_nai("吐か", "ハカ");
        assert!(nasou_variants(&[(-7.0, &adj), (-9.0, &verb)], ("", ""), &HashSet::new(), NASOU_MAX, &no_dict).is_empty());
        assert_eq!(nasou_variants(&[(-9.0, &verb)], ("", ""), &HashSet::new(), NASOU_MAX, &no_dict).len(), 1);
    }
}
