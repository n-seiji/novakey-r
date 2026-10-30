//! 動的に作る辞書エントリ (`DicdataStore.getWiseDicdata` と `JapaneseNumber.swift`)。
//!
//! かな数字 (いち -> 1 / 一)、ASCII 整数 (1234 と漢数字)、1 文字のひらがな・カタカナ、記号の全半角と異体字。
//! 英単語 (`keyboardLanguage == .en_US`) は IME では使わないので移植しない。
#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::OnceLock;

use super::dict::to_katakana;
use super::elem::{count_graphemes, Elem, CID_NUMBER, CID_PROPER_NOUN, CID_SYMBOL, MID_GENERAL, MID_NUMBER, MID_SMALL_NUMBER};

/// 全角記号 -> 半角記号の対応。`DicdataStore.fullwidthToHalfwidth` を ICU の結果で固定した表
/// (tools/golden/width_map.tsv)。〜 は半角に変換されない。
const FULL_HALF: [(char, char); 29] = [
    ('＋', '+'),
    ('ー', 'ｰ'),
    ('＊', '*'),
    ('＝', '='),
    ('・', '･'),
    ('！', '!'),
    ('＃', '#'),
    ('％', '%'),
    ('＆', '&'),
    ('＇', '\''),
    ('＂', '"'),
    ('〜', '〜'),
    ('｜', '|'),
    ('￡', '£'),
    ('＄', '$'),
    ('￥', '¥'),
    ('＠', '@'),
    ('｀', '`'),
    ('；', ';'),
    ('：', ':'),
    ('＜', '<'),
    ('＞', '>'),
    ('，', ','),
    ('．', '.'),
    ('＼', '\\'),
    ('／', '/'),
    ('＿', '_'),
    ('￣', '¯'),
    ('－', '-'),
];

fn full_to_half(c: char) -> char {
    FULL_HALF.iter().find(|&&(f, _)| f == c).map_or(c, |&(_, h)| h)
}

fn half_to_full(c: char) -> Option<char> {
    FULL_HALF.iter().find(|&&(_, h)| h == c).map(|&(f, _)| f)
}

/// 弱い類似にある記号のグループ (`weakRelatingSymbolGroups`)。宣言順が出力順に効く。
const WEAK_GROUPS: &[&[&str]] = &[
    &["高", "髙"],
    &["斎", "斉", "齋", "齊"],
    &["澤", "沢"],
    &["気", "氣"],
    &["澁", "渋"],
    &["対", "對"],
    &["辻", "辻\u{E0100}"],
    &["禰\u{E0100}", "禰"],
    &["煉\u{E0101}", "煉"],
    &["崎", "﨑"],
    &["栄", "榮"],
    &["吉", "𠮷"],
    &["橋", "𣘺", "槗", "𫞎"],
    &["浜", "濱", "濵"],
    &["鴎", "鷗"],
    &["学", "學"],
    &["角", "⻆"],
    &["亀", "龜"],
    &["桜", "櫻"],
    &["真", "眞"],
    &["☆", "★", "♡", "☾", "☽"],
    &["^", "＾"],
    &["¥", "$", "¢", "€", "£", "₿"],
    &["%", "‰"],
    &["°", "℃", "℉"],
    &["◯"],
    &["*", "※", "✳\u{FE0E}", "✴\u{FE0E}"],
    &["、", "。", "，", "．", "・", "…", "‥", "•"],
    &["+", "±", "⊕"],
    &["×", "❌", "✖\u{FE0F}"],
    &["÷", "➗"],
    &["<", "≦", "≪", "〈", "《", "‹", "«"],
    &[">", "≧", "≫", "〉", "》", "›", "»"],
    &["「", "『", "（", "［", "《", "【"],
    &["」", "』", "）", "］", "》", "】"],
    &["「」", "『』", "（）", "［］", "《》", "【】"],
    &["(", "{", "<", "["],
    &[")", "}", ">", "]"],
    &["()", "{}", "<>", "[]"],
    &["’", "“", "”", "„", "\"", "`", "'"],
    &["\"\"\"", "'''", "```"],
    &["=", "≒", "≠", "≡"],
    &[":", ";"],
    &["!", "❗\u{FE0F}", "❣\u{FE0F}", "‼\u{FE0E}", "⁉\u{FE0E}", "❕", "‼\u{FE0F}", "⁉\u{FE0F}", "¡"],
    &["?", "❓", "⁉\u{FE0E}", "⁇", "❔", "⁉\u{FE0F}", "¿"],
    &["〒", "〠", "℡", "☎\u{FE0E}"],
    &["々", "ヾ", "ヽ", "ゝ", "ゞ", "〃", "仝", "〻"],
    &["〆", "〼", "ゟ", "ヿ"],
    &["♂", "♀", "⚢", "⚣", "⚤", "⚥", "⚦", "⚧", "⚨", "⚩", "⚪\u{FE0E}", "⚲"],
    &["→", "↑", "←", "↓", "↙\u{FE0E}", "↖\u{FE0E}", "↘\u{FE0E}", "↗\u{FE0E}", "↔\u{FE0E}", "↕\u{FE0E}", "↪\u{FE0E}", "↩\u{FE0E}", "⇆"],
    &["♯", "♭", "♪", "♮", "♫", "♬", "♩", "𝄞", "𝄞"],
    &["√", "∛", "∜"],
];

fn weak_lookup() -> &'static HashMap<&'static str, Vec<&'static str>> {
    static MAP: OnceLock<HashMap<&'static str, Vec<&'static str>>> = OnceLock::new();
    MAP.get_or_init(|| {
        let mut map: HashMap<&'static str, Vec<&'static str>> = HashMap::new();
        for group in WEAK_GROUPS {
            for &c in *group {
                map.entry(c).or_default().extend(group.iter().copied());
            }
        }
        // `Array.uniqued()`: 最初に現れた順を保って重複を除く
        for v in map.values_mut() {
            let mut seen: Vec<&str> = Vec::with_capacity(v.len());
            for &s in v.iter() {
                if !seen.contains(&s) {
                    seen.push(s);
                }
            }
            *v = seen;
        }
        map
    })
}

/// `Character.isNumber` に当たるもの。Swift は先頭スカラーの Numeric_Type が none でないかを見る。
/// Rust の `is_numeric` (Nd / Nl / No) に、Numeric_Type が付く漢数字を足す。
pub fn is_number(c: char) -> bool {
    c.is_numeric() || matches!(c, '一' | '二' | '三' | '四' | '五' | '六' | '七' | '八' | '九' | '十' | '百' | '千' | '万' | '億' | '兆' | '零' | '壱' | '弐' | '参' | '拾' | '京' | '〇' | '廿' | '卅' | '卌' | '皕' | '陌' | '阡' | '萬' | '貳' | '叁' | '肆' | '伍' | '陸' | '柒' | '捌' | '玖' | '仟' | '佰')
}

/// カタカナ -> ひらがな (`String.toHiragana`)。
pub fn to_hiragana(s: &str) -> String {
    s.chars()
        .map(|c| match c as u32 {
            0x30A1..=0x30F6 => char::from_u32(c as u32 - 0x60).unwrap_or(c),
            _ => c,
        })
        .collect()
}

// ---------------------------------------------------------------- 漢数字

const KANSUJI_DIGITS: [&str; 10] = ["〇", "一", "二", "三", "四", "五", "六", "七", "八", "九"];

/// 4 桁以下 (1..=9999) の漢数字。先頭の 1 は省く (千・百・十)。
fn group_to_kansuji(g: u32) -> String {
    let mut s = String::new();
    for (unit, v) in [("千", g / 1000), ("百", g / 100 % 10), ("十", g / 10 % 10)] {
        match v {
            0 => {}
            1 => s.push_str(unit),
            d => {
                s.push_str(KANSUJI_DIGITS[d as usize]);
                s.push_str(unit);
            }
        }
    }
    let one = g % 10;
    if one != 0 {
        s.push_str(KANSUJI_DIGITS[one as usize]);
    }
    s
}

/// `NumberFormatter` (spellOut, ja-JP) の結果。`|n| <= 1e12` で使う前提。
/// 10000 -> 一万、1000000 -> 百万、1100 -> 千百、-5 -> マイナス五、0 -> 〇。
pub fn kansuji(n: i64) -> String {
    if n == 0 {
        return KANSUJI_DIGITS[0].to_string();
    }
    let mut out = String::new();
    if n < 0 {
        out.push_str("マイナス");
    }
    let mut v = n.unsigned_abs();
    let mut groups = [0u32; 6];
    let mut count = 0;
    while v > 0 {
        groups[count] = (v % 10000) as u32;
        v /= 10000;
        count += 1;
    }
    const UNITS: [&str; 6] = ["", "万", "億", "兆", "京", "垓"];
    for idx in (0..count).rev() {
        let g = groups[idx];
        if g == 0 {
            continue;
        }
        if idx > 0 && g == 1 {
            out.push_str("一");
        } else {
            out.push_str(&group_to_kansuji(g));
        }
        out.push_str(UNITS[idx]);
    }
    out
}

// ---------------------------------------------------------------- かな数字

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tok {
    Ichi,
    Ni,
    San,
    Yon,
    Go,
    Roku,
    Nana,
    Hachi,
    Kyuu,
    Rei,
    Juu,
    Hyaku,
    Sen,
    Man,
    Oku,
    Chou,
    End,
    Err,
}

impl Tok {
    fn is_number(self) -> bool {
        self == Tok::End || self.digit().is_some()
    }
    fn is_not_number(self) -> bool {
        matches!(self, Tok::Juu | Tok::Hyaku | Tok::Sen | Tok::Man | Tok::Oku | Tok::Chou | Tok::Err | Tok::End)
    }
    fn to_kanji(self) -> &'static str {
        match self {
            Tok::Juu => "十",
            Tok::Hyaku => "百",
            Tok::Sen => "千",
            Tok::Man => "万",
            Tok::Oku => "億",
            Tok::Chou => "兆",
            _ => self.digit().map_or("", |d| KANSUJI_DIGITS[usize::from(d)]),
        }
    }
    fn max_digit(self) -> Option<usize> {
        match self {
            Tok::End => Some(1),
            Tok::Man => Some(2),
            Tok::Oku => Some(3),
            Tok::Chou => Some(4),
            _ => None,
        }
    }
    /// 数字トークンの値 (0..=9)
    fn digit(self) -> Option<u8> {
        match self {
            Tok::Ichi => Some(1),
            Tok::Ni => Some(2),
            Tok::San => Some(3),
            Tok::Yon => Some(4),
            Tok::Go => Some(5),
            Tok::Roku => Some(6),
            Tok::Nana => Some(7),
            Tok::Hachi => Some(8),
            Tok::Kyuu => Some(9),
            Tok::Rei => Some(0),
            _ => None,
        }
    }
}

/// Swift の `parseLiteral` の先読みイテレータ。`next()` は先読み値を返してから 1 つ進める。
struct Scalars {
    chars: Vec<char>,
    idx: usize,
    lookahead: Option<char>,
}

impl Scalars {
    fn new(input: &str) -> Self {
        let chars: Vec<char> = input.chars().collect();
        let lookahead = chars.first().copied();
        Self { idx: usize::from(lookahead.is_some()), chars, lookahead }
    }

    fn next(&mut self) -> Option<char> {
        let current = self.lookahead;
        self.lookahead = self.chars.get(self.idx).copied();
        if self.lookahead.is_some() {
            self.idx += 1;
        }
        current
    }
}

/// `parseLiteral`: カタカナの読みを数トークンにする。解釈できなければ末尾が `Err`。
///
/// Swift 版は「シ」の次が「チ」でないとき、読み進めた 1 文字を先読みに戻す際にその次の 1 文字を失う
/// (例: シゴト -> よん, ご で「ト」が消えて 45)。同じ結果にするため、イテレータの動きごと写している。
fn parse_literal(input: &str) -> Vec<Tok> {
    let mut it = Scalars::new(input);
    let mut tokens = Vec::new();
    while let Some(u0) = it.next() {
        let tok = match u0 {
            'イ' => match it.next() {
                Some('チ') | Some('ッ') => Tok::Ichi,
                _ => return err(tokens),
            },
            'オ' => match it.next() {
                Some('ク') => Tok::Oku,
                _ => return err(tokens),
            },
            'キ' => match it.next() {
                Some('ュ') => match it.next() {
                    Some('ウ') => Tok::Kyuu,
                    _ => return err(tokens),
                },
                _ => return err(tokens),
            },
            'ク' => Tok::Kyuu,
            'ゴ' => Tok::Go,
            'サ' => match it.next() {
                Some('ン') => Tok::San,
                _ => return err(tokens),
            },
            'シ' => match it.next() {
                Some('チ') => Tok::Nana,
                Some(u1) => {
                    it.lookahead = Some(u1);
                    Tok::Yon
                }
                None => Tok::Yon,
            },
            'ジ' => match it.next() {
                Some('ュ') => match it.next() {
                    Some('ウ') | Some('ッ') => Tok::Juu,
                    _ => return err(tokens),
                },
                _ => return err(tokens),
            },
            'セ' => match it.next() {
                Some('ン') => Tok::Sen,
                _ => return err(tokens),
            },
            'ゼ' => match it.next() {
                Some('ロ') => Tok::Rei,
                Some('ン') => Tok::Sen,
                _ => return err(tokens),
            },
            'チ' => match it.next() {
                Some('ョ') => match it.next() {
                    Some('ウ') => Tok::Chou,
                    _ => return err(tokens),
                },
                _ => return err(tokens),
            },
            'ナ' => match it.next() {
                Some('ナ') => Tok::Nana,
                _ => return err(tokens),
            },
            'ニ' => Tok::Ni,
            'ハ' => match it.next() {
                Some('チ') | Some('ッ') => Tok::Hachi,
                _ => return err(tokens),
            },
            'ヒ' | 'ビ' | 'ピ' => match it.next() {
                Some('ャ') => match it.next() {
                    Some('ク') => Tok::Hyaku,
                    _ => return err(tokens),
                },
                _ => return err(tokens),
            },
            'マ' => match it.next() {
                Some('ン') => Tok::Man,
                Some('ル') => Tok::Rei,
                _ => return err(tokens),
            },
            'ヨ' => match it.next() {
                Some('ン') => Tok::Yon,
                _ => return err(tokens),
            },
            'レ' => match it.next() {
                Some('イ') => Tok::Rei,
                _ => return err(tokens),
            },
            'ロ' => match it.next() {
                Some('ク') | Some('ッ') => Tok::Roku,
                _ => return err(tokens),
            },
            _ => return err(tokens),
        };
        tokens.push(tok);
    }
    tokens.push(Tok::End);
    tokens
}

fn err(mut tokens: Vec<Tok>) -> Vec<Tok> {
    tokens.push(Tok::Err);
    tokens
}

/// 4 桁ぶんの (千, 百, 十, 一) の数字 (0..=9)
type Stack = [u8; 4];

/// `parseTokens`。空なら解釈できない。
fn parse_tokens(tokens: &[Tok]) -> Vec<Stack> {
    let mut max_digits: Option<usize> = None;
    let mut result: Vec<Stack> = Vec::new();
    let mut stack: Stack = [0; 4];
    let mut curnum: Option<u8> = None;
    for &token in tokens {
        if let Some(d) = token.digit() {
            if curnum.is_some() {
                return Vec::new();
            }
            curnum = Some(d);
            continue;
        }
        match token {
            Tok::Juu => {
                stack[2] = curnum.unwrap_or(1);
                curnum = None;
            }
            Tok::Hyaku => {
                stack[1] = curnum.unwrap_or(1);
                curnum = None;
            }
            Tok::Sen => {
                stack[0] = curnum.unwrap_or(1);
                curnum = None;
            }
            Tok::End | Tok::Man | Tok::Oku | Tok::Chou => {
                stack[3] = curnum.unwrap_or(0);
                let md = token.max_digit().unwrap_or(1);
                match max_digits {
                    Some(m) => {
                        if m <= md {
                            return Vec::new();
                        }
                        result[m - md] = stack;
                    }
                    None => {
                        max_digits = Some(md);
                        result = vec![[0; 4]; md];
                        result[0] = stack;
                    }
                }
                curnum = None;
                stack = [0; 4];
            }
            _ => {}
        }
    }
    result
}

/// `getJapaneseNumberDicdata`: カタカナの読みが日本語の数なら 漢数字 と アラビア数字 の 2 エントリ。
fn japanese_number_dicdata(head: &str, out: &mut Vec<Elem>) {
    let tokens = parse_literal(head);
    if tokens.iter().any(|&t| t == Tok::Err) {
        return;
    }
    let kanji: String = tokens.iter().map(|t| t.to_kanji()).collect();
    let roman: String = if tokens.iter().all(|t| t.is_number()) {
        tokens.iter().filter_map(|t| t.digit()).map(|d| char::from(b'0' + d)).collect()
    } else if tokens.iter().all(|t| t.is_not_number()) {
        return;
    } else {
        let result = parse_tokens(&tokens);
        if result.is_empty() {
            return;
        }
        let mut chars = String::new();
        for stack in &result {
            let digits = |from: usize| -> String { stack[from..].iter().map(|d| char::from(b'0' + d)).collect() };
            if chars.is_empty() {
                if stack[0] != 0 {
                    chars.push_str(&digits(0));
                } else if stack[1] != 0 {
                    chars.push_str(&digits(1));
                } else if stack[2] != 0 {
                    chars.push_str(&digits(2));
                } else if stack[3] != 0 {
                    chars.push_str(&digits(3));
                } else {
                    return;
                }
            } else {
                chars.push_str(&digits(0));
            }
        }
        chars
    };
    let head_count = count_graphemes(head) as f32;
    let roman_count = count_graphemes(&roman) as f32;
    out.push(Elem::new(kanji, head.to_string(), CID_NUMBER, CID_NUMBER, MID_NUMBER, -17.0 + head_count / 3.0));
    out.push(Elem::new(roman, head.to_string(), CID_NUMBER, CID_NUMBER, MID_NUMBER, -16.0 + 4.0 / roman_count));
}

// ---------------------------------------------------------------- 入口

/// `getWiseDicdata`。`segment` は `full_text[range]` をカタカナ化した文字列、`range` は surface の範囲。
pub fn wise_dicdata(segment: &str, range: std::ops::Range<usize>, full_text: &[char]) -> Vec<Elem> {
    let mut result: Vec<Elem> = Vec::new();
    japanese_number_dicdata(segment, &mut result);

    // 直前・直後が数字なら ASCII 整数は作らない
    {
        let prev_is_number = range.start > 0 && is_number(full_text[range.start - 1]);
        let next_is_number = range.end < full_text.len() && is_number(full_text[range.end]);
        if !(prev_is_number || next_is_number) {
            if let Ok(number) = segment.parse::<i64>() {
                result.push(Elem::new(segment.to_string(), segment.to_string(), CID_NUMBER, CID_NUMBER, MID_SMALL_NUMBER, -14.0));
                if (number as f64) <= 1e12 && -1e12 <= (number as f64) {
                    result.push(Elem::new(kansuji(number), segment.to_string(), CID_NUMBER, CID_NUMBER, MID_SMALL_NUMBER, -16.0));
                }
            }
        }
    }

    // 1 文字のとき: ひらがな・カタカナ
    let mut it = segment.chars();
    let single = match (it.next(), it.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    };
    // `convertTarget.count == 1` は grapheme 数。1 スカラーの場合に加え、結合文字つきの 1 文字も数える
    let is_single_grapheme = single.is_some() || (segment.chars().count() > 1 && count_graphemes(segment) == 1);
    if is_single_grapheme {
        let katakana = to_katakana(segment);
        let hiragana = to_hiragana(segment);
        if katakana == hiragana {
            result.push(Elem::new(katakana.clone(), katakana, CID_PROPER_NOUN, CID_PROPER_NOUN, MID_GENERAL, -14.0));
        } else {
            result.push(Elem::new(hiragana, katakana.clone(), CID_PROPER_NOUN, CID_PROPER_NOUN, MID_GENERAL, -13.0));
            result.push(Elem::new(katakana.clone(), katakana, CID_PROPER_NOUN, CID_PROPER_NOUN, MID_GENERAL, -14.0));
        }
    }
    // 記号の全半角
    if let Some(first) = single {
        let mut value = -14.0f32;
        let hs = full_to_half(first);
        if hs != first {
            result.push(Elem::new(segment.to_string(), segment.to_string(), CID_SYMBOL, CID_SYMBOL, MID_GENERAL, value));
            value -= 5.0;
            result.push(Elem::new(hs.to_string(), segment.to_string(), CID_SYMBOL, CID_SYMBOL, MID_GENERAL, value));
            value -= 5.0;
        }
        if let Some(fs) = half_to_full(first) {
            if fs != first {
                result.push(Elem::new(segment.to_string(), segment.to_string(), CID_SYMBOL, CID_SYMBOL, MID_GENERAL, value));
                value -= 5.0;
                result.push(Elem::new(fs.to_string(), segment.to_string(), CID_SYMBOL, CID_SYMBOL, MID_GENERAL, value));
            }
        }
    }
    // 弱い類似の記号 (異体字・矢印など)
    if let Some(group) = weak_lookup().get(segment) {
        let mut value = -34.0f32;
        for &symbol in group {
            if symbol == segment {
                continue;
            }
            result.push(Elem::new(symbol.to_string(), segment.to_string(), CID_SYMBOL, CID_SYMBOL, MID_GENERAL, value));
            value -= 5.0;
            let mut sc = symbol.chars();
            if let (Some(c), None) = (sc.next(), sc.next()) {
                if let Some(fs) = half_to_full(c) {
                    if fs != c {
                        result.push(Elem::new(fs.to_string(), segment.to_string(), CID_SYMBOL, CID_SYMBOL, MID_GENERAL, value));
                        value -= 5.0;
                    }
                }
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kansuji_matches_spellout_examples() {
        assert_eq!(kansuji(0), "〇");
        assert_eq!(kansuji(10), "十");
        assert_eq!(kansuji(100), "百");
        assert_eq!(kansuji(1000), "千");
        assert_eq!(kansuji(1100), "千百");
        assert_eq!(kansuji(10000), "一万");
        assert_eq!(kansuji(10001), "一万一");
        assert_eq!(kansuji(1_000_000), "百万");
        assert_eq!(kansuji(100_000_000), "一億");
        assert_eq!(kansuji(1_000_000_000_000), "一兆");
        assert_eq!(kansuji(-5), "マイナス五");
        assert_eq!(kansuji(1998), "千九百九十八");
    }

    #[test]
    fn kansuji_matches_golden_table() {
        let tsv = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tools/golden/kansuji.tsv")).unwrap();
        let mut checked = 0;
        for line in tsv.lines().skip(1) {
            let cols: Vec<&str> = line.split('\t').collect();
            assert_eq!(cols.len(), 3, "{line}");
            let (input, kan) = (cols[0], cols[2]);
            let parsed = input.parse::<i64>().ok();
            match (parsed, kan) {
                (None, "-") => {}
                (Some(n), "-") => assert!(n.abs() > 1_000_000_000_000, "{input} は漢数字なしのはず"),
                (Some(n), k) => {
                    assert_eq!(cols[1].parse::<i64>().unwrap(), n, "{input}");
                    assert!(n.abs() <= 1_000_000_000_000);
                    assert_eq!(kansuji(n), k, "{input}");
                }
                (None, k) => panic!("{input} は Int として読めないのに漢数字 {k} がある"),
            }
            checked += 1;
        }
        assert!(checked > 10000, "{checked}");
    }

    #[test]
    fn int_parse_rules_match_swift() {
        for ok in ["+5", "-0", "007", "5"] {
            assert!(ok.parse::<i64>().is_ok(), "{ok}");
        }
        for ng in ["", "+", "-", " 5", "5 ", "1,000", "１２", "--5", "9223372036854775808"] {
            assert!(ng.parse::<i64>().is_err(), "{ng}");
        }
    }

    fn numbers(head: &str) -> Vec<(String, f32)> {
        let mut v = Vec::new();
        japanese_number_dicdata(head, &mut v);
        v.into_iter().map(|e| (e.word.clone(), e.value())).collect()
    }

    #[test]
    fn kana_numbers_produce_kanji_and_arabic() {
        let n = numbers("イチ");
        assert_eq!(n[0].0, "一");
        assert_eq!(n[1].0, "1");
        let n = numbers("ニセンニジュウイチ");
        assert_eq!(n[0].0, "二千二十一");
        assert_eq!(n[1].0, "2021");
        let n = numbers("ヒャクニジュウ");
        assert_eq!(n[1].0, "120");
        let n = numbers("イチマン");
        assert_eq!(n[0].0, "一万");
        assert_eq!(n[1].0, "10000");
        let n = numbers("センニヒャク");
        assert_eq!(n[1].0, "1200");
        assert!(numbers("センマン").is_empty(), "位だけのトークン列は数にならない");
        assert!(numbers("ジュウ").is_empty(), "単独の位は数にならない");
        assert!(numbers("コンニチハ").is_empty());
        let n = numbers("シ");
        assert_eq!(n[1].0, "4");
        assert!(numbers("コンニチハ").is_empty());
        let n = numbers("シチ");
        assert_eq!(n[1].0, "7");
        // Swift 版の先読みの癖: 「シ」の次が「チ」でないと、その次の 1 文字を失う
        let n = numbers("シゴト");
        assert_eq!(n[0].0, "四五");
        assert_eq!(n[1].0, "45");
        let n = numbers("シゴ");
        assert_eq!(n[1].0, "45");
    }

    #[test]
    fn number_values_follow_swift_formula() {
        let n = numbers("イチ");
        assert_eq!(n[0].1, -17.0 + 2.0f32 / 3.0);
        assert_eq!(n[1].1, -16.0 + 4.0f32 / 1.0);
        // base value は min(0, v): +4 - 16 = -12
        assert_eq!(n[1].1, -12.0);
    }

    fn wise(s: &str) -> Vec<Elem> {
        let chars: Vec<char> = s.chars().collect();
        wise_dicdata(s, 0..chars.len(), &chars)
    }

    #[test]
    fn single_kana_gets_hiragana_and_katakana() {
        let r = wise("カ");
        assert_eq!(r.len(), 2);
        assert_eq!((r[0].word.as_str(), r[0].value()), ("か", -13.0));
        assert_eq!((r[1].word.as_str(), r[1].value()), ("カ", -14.0));
        let r = wise("k");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].value(), -14.0);
    }

    #[test]
    fn symbols_get_width_variants_with_decreasing_scores() {
        let r = wise("＋");
        let words: Vec<(&str, f32)> = r.iter().map(|e| (e.word.as_str(), e.value())).collect();
        // 単独の記号 -14 -> 半角 -19、そして弱い類似グループ
        assert_eq!(&words[0..3], &[("＋", -14.0), ("＋", -14.0), ("+", -19.0)]);
        let r = wise("+");
        let words: Vec<(&str, f32)> = r.iter().map(|e| (e.word.as_str(), e.value())).collect();
        assert!(words.contains(&("＋", -19.0)), "{words:?}");
        assert!(words.contains(&("±", -34.0)), "{words:?}");
    }

    #[test]
    fn ascii_integer_is_suppressed_next_to_digits() {
        let chars: Vec<char> = "12".chars().collect();
        let mid = wise_dicdata("1", 0..1, &chars);
        assert!(mid.is_empty() || mid.iter().all(|e| e.mid != MID_SMALL_NUMBER as usize), "隣が数字なら整数は作らない");
        let alone = wise("12");
        assert_eq!(alone.iter().filter(|e| e.mid == MID_SMALL_NUMBER).count(), 2);
    }
}
