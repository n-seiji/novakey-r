#![forbid(unsafe_code)]

// english_split.rs - 英単語のあとに助詞などが続く打鍵列 (artifactni) を、英単語とかなに分ける。
// かなの単位に頼らず打鍵列を切り直す (artifactwo の two は とぉ ではなく t + を)。

use crate::romaji_converter::RomajiConverter;

/// 英単語の直後に来てよいかなの始まり。include の de や partner の ne を切らないよう、よく使う助詞などに限る。
const TAILS: &[&str] = &[
    "に", "を", "が", "は", "で", "と", "の", "も", "へ", "から", "まで", "より", "って", "する", "して",
    "した", "しま", "され", "させ", "でき",
];

/// 辞書の英単語を頭に使う最小の長さ (Composer::is_typing_english の MIN_LEN と同じ)。
const DICTIONARY_MIN_LEN: usize = 4;

/// 助詞で始まらない続き (loginjinihiraku の じに…) を許すとき、頭に含まれているべき字。
/// 日本語のローマ字ではほぼ使わない (l, x は小文字かなだけ)。time + い (timei) のような日本語の打ち間違いを切らないための歯止め。
const FOREIGN_LETTERS: [char; 4] = ['l', 'q', 'v', 'x'];

/// 助詞で始まらない続きの打鍵に含まれていたら切らない字。続きが外来語の綴りの一部だと見なす。
const FOREIGN_TAIL_LETTERS: [char; 5] = ['l', 'q', 'v', 'x', 'c'];

/// 前のかなが i 段のとき (きょう, じゃ) に限り続けてよい小さいかな。
const YOUON: [char; 3] = ['ゃ', 'ゅ', 'ょ'];
const I_ROW: [char; 12] = ['き', 'し', 'ち', 'に', 'ひ', 'み', 'り', 'ぎ', 'じ', 'ぢ', 'び', 'ぴ'];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Split {
    pub head: String,
    /// かなの部分の (打鍵, かな) の組。
    pub tail: Vec<(String, String)>,
    /// かなの部分の未確定のローマ字。
    pub pending: String,
}

impl Split {
    /// かなの部分の読み (未確定のローマ字は含めない)。
    pub fn tail_kana(&self) -> String {
        self.tail.iter().map(|(_, kana)| kana.as_str()).collect()
    }

    pub fn display(&self) -> String {
        std::iter::once(self.head.as_str())
            .chain(self.tail.iter().map(|(_, kana)| kana.as_str()))
            .chain(std::iter::once(self.pending.as_str()))
            .collect()
    }
}

/// 打鍵列をかなにした (打鍵, かな) の組と、残ったローマ字。finished なら残りも確定する。
fn convert(raw: &str, finished: bool) -> (Vec<(String, String)>, String) {
    let mut conv = RomajiConverter::new();
    let mut units = Vec::new();
    let mut typed = String::new();
    for ch in raw.chars() {
        typed.push(ch);
        if let Some(kana) = conv.process_input(ch) {
            let carry = conv.buffer();
            let split = typed.strip_suffix(carry).map_or(typed.len(), str::len);
            let rest = typed.split_off(split);
            units.push((std::mem::replace(&mut typed, rest), kana));
        }
    }
    if let Some(kana) = finished.then(|| conv.flush()).flatten() {
        units.push((std::mem::take(&mut typed), kana));
    }
    (units, conv.buffer().to_string())
}

fn has_ascii(units: &[(String, String)]) -> bool {
    units.iter().any(|(_, kana)| kana.chars().any(|c| c.is_ascii_alphabetic()))
}

/// 辞書の英単語を頭に使ってよいか。日本語の打ちかけと紛れないよう 4 文字以上で、かな読みが日本語の語なら使わない。
fn dictionary_head(head: &str, is_word: &impl Fn(&str) -> bool, is_japanese: &impl Fn(&str) -> bool) -> bool {
    head.len() >= DICTIONARY_MIN_LEN && is_word(head) && not_japanese(head, is_japanese)
}

/// 頭のかな読みが日本語の語でないか。かなにしきれない語 (commit の こっみt) は日本語の語になりえない。
fn not_japanese(head: &str, is_japanese: &impl Fn(&str) -> bool) -> bool {
    let (units, pending) = convert(head, true);
    let kana: String = units.iter().map(|(_, k)| k.as_str()).collect();
    has_ascii(&units) || !pending.is_empty() || !is_japanese(&kana)
}

/// 続きのかなが、外来語の綴りの切れ端ではなく日本語として自然か。ぁぃぅぇぉゎは含まず、ゃゅょは i 段のあとだけ。
fn natural_kana(tail: &[(String, String)]) -> bool {
    let mut prev = None;
    for c in tail.iter().flat_map(|(_, kana)| kana.chars()) {
        let small = matches!(c, 'ぁ' | 'ぃ' | 'ぅ' | 'ぇ' | 'ぉ' | 'ゎ');
        let bad_youon = YOUON.contains(&c) && !prev.is_some_and(|p| I_ROW.contains(&p));
        if small || bad_youon {
            return false;
        }
        prev = Some(c);
    }
    true
}

/// raw を 英単語 + 助詞などで始まるかな に分ける。分けられなければ None。
/// 英単語と見なすのは、全体がかなにしきれない (artifact の rt)、英語として確定したことのある語、
/// または辞書の英単語 (4 文字以上。merge, commit) のいずれか。
/// ただし、そのかな読みが日本語の語なら見なさない (made → まで)。辞書の英単語にも確定済みの語 (knows) にも掛ける
/// (done を英語で選んだ後でも donewo を done + を にしない)。かなにしきれない語 (git) は確定済みなら分ける。
/// 上の分け方で見つからず、全体がかなにしきれて、頭に l q v x を含む語 (login) のときに限り、助詞で始まらない続きでも分ける (loginjinihiraku → login + じに…)。
/// 全体がまだ英単語の途中 (informa) なら分けない。
/// 「全体がかなにしきれる」は、打ちかけ (finished でない) なら末尾の未確定ローマ字 (loginj の j) を含んでよい。
/// finished は Enter などで打ち終えたとき。途中なら末尾の未確定ローマ字を許す。
pub fn split(
    raw: &str,
    finished: bool,
    knows: impl Fn(&str) -> bool,
    is_dictionary_word: impl Fn(&str) -> bool,
    is_word_prefix: impl Fn(&str) -> bool,
    is_japanese_reading: impl Fn(&str) -> bool,
) -> Option<Split> {
    if raw.len() < 3 || !raw.bytes().all(|b| b.is_ascii_lowercase()) {
        return None;
    }
    let (whole, _) = convert(raw, true);
    let foreign = has_ascii(&whole);
    let split_at = |i: usize| {
        let (head, rest) = raw.split_at(i);
        let (tail, pending) = convert(rest, finished);
        if has_ascii(&tail) || (finished && !pending.is_empty()) {
            return None;
        }
        let mut prefix = String::new();
        let starts_with_tail = tail.iter().any(|(_, kana)| {
            prefix.push_str(kana);
            TAILS.contains(&prefix.as_str())
        });
        starts_with_tail.then(|| Split { head: head.to_string(), tail, pending })
    };
    let is_word = |head: &str| (knows(head) && not_japanese(head, &is_japanese_reading)) || dictionary_head(head, &is_dictionary_word, &is_japanese_reading);
    // 助詞で始まらない続きを許す分け方。外来語の綴りらしい頭と、日本語として自然な続きだけに絞る。
    // 打ちかけの未確定ローマ字 (loginj の j) は、かなにしきれない印にしない。
    // 全体の変換で かな 1 つ分の途中 (leann の lean|n は ん=nn の内側) になる位置では切らない。
    let relaxed_at = |boundaries: &[usize], i: usize| {
        let (head, rest) = raw.split_at(i);
        if !boundaries.contains(&i) || !head.contains(FOREIGN_LETTERS) || rest.contains(FOREIGN_TAIL_LETTERS) {
            return None;
        }
        let word = (knows(head) && head.len() >= DICTIONARY_MIN_LEN && not_japanese(head, &is_japanese_reading))
            || dictionary_head(head, &is_dictionary_word, &is_japanese_reading);
        if !word {
            return None;
        }
        let (tail, pending) = convert(rest, finished);
        let has_unit = !tail.is_empty() || !pending.is_empty();
        if has_ascii(&tail) || (finished && !pending.is_empty()) || !has_unit || !natural_kana(&tail) {
            return None;
        }
        Some(Split { head: head.to_string(), tail, pending })
    };
    let relaxed = || {
        if !raw.contains(FOREIGN_LETTERS) || is_word_prefix(raw) || knows(raw) {
            return None;
        }
        let (units, _) = convert(raw, finished);
        if has_ascii(&units) {
            return None;
        }
        let boundaries: Vec<usize> = units
            .iter()
            .scan(0, |end, (typed, _)| {
                *end += typed.len();
                Some(*end)
            })
            .collect();
        (2..raw.len()).rev().find_map(|i| relaxed_at(&boundaries, i))
    };
    // 語と分かる頭を先に、長い順で試す。語でない頭 (子音で終わる外来の綴り) は短い順で試す。長い順だと
    // artifactwokousinnsite を artifactwokousinn + して と切ってしまうため。
    (2..raw.len())
        .rev()
        .find_map(|i| is_word(&raw[..i]).then(|| split_at(i)).flatten())
        .or_else(|| {
            (2..raw.len()).find_map(|i| {
                let consonant_end = !raw[..i].ends_with(['a', 'i', 'u', 'e', 'o']);
                (consonant_end && foreign).then(|| split_at(i)).flatten()
            })
        })
        .or_else(relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(raw: &str) -> Option<String> {
        split(raw, false, |_| false, |_| false, |_| false, |_| false).map(|s| s.display())
    }

    #[test]
    fn 英単語のあとの助詞をかなにする() {
        assert_eq!(shown("artifactni").as_deref(), Some("artifactに"));
        assert_eq!(shown("artifactwotsukau").as_deref(), Some("artifactをつかう"));
        assert_eq!(shown("artifactnisuru").as_deref(), Some("artifactにする"));
    }

    #[test]
    fn 助詞のあとに長い日本語が続いても英単語の直後で分ける() {
        assert_eq!(shown("artifactwokousinnsite").as_deref(), Some("artifactをこうしんして"));
        assert_eq!(dict_split("artifactwokousinnsite", &["artifact"], &[]).as_deref(), Some("artifactをこうしんして"));
        assert_eq!(shown("githubwokousinnsite").as_deref(), Some("githubをこうしんして"));
    }

    #[test]
    fn 途中のローマ字は残して表示する() {
        assert_eq!(shown("artifactnis").as_deref(), Some("artifactにs"));
    }

    #[test]
    fn 打ち終えたら残りのローマ字が変換できないものは分けない() {
        assert_eq!(split("artifactnish", true, |_| false, |_| false, |_| false, |_| false), None);
        let done = split("artifactnin", true, |_| false, |_| false, |_| false, |_| false).unwrap();
        assert_eq!(done.display(), "artifactにん");
    }

    #[test]
    fn 英単語の中の音は助詞と見なさない() {
        assert_eq!(shown("include"), None);
        assert_eq!(shown("partner"), None);
        assert_eq!(shown("standard"), None);
    }

    #[test]
    fn かなにしきれる日本語は分けない() {
        assert_eq!(shown("kitto"), None);
        assert_eq!(shown("kantanni"), None);
        assert_eq!(shown("kyouhaishaniiku"), None);
    }

    #[test]
    fn 英語として確定したことのある語は母音で終わっても詰まりがなくても分ける() {
        assert_eq!(split("mergeni", false, |w| w == "merge", |_| false, |_| false, |_| false).map(|s| s.display()).as_deref(), Some("mergeに"));
        assert_eq!(split("openni", false, |w| w == "open", |_| false, |_| false, |_| false).map(|s| s.display()).as_deref(), Some("openに"));
        assert_eq!(split("openni", false, |_| false, |_| false, |_| false, |_| false), None);
    }

    fn dict_split(raw: &str, words: &[&str], japanese: &[&str]) -> Option<String> {
        split(raw, false, |_| false, |w| words.contains(&w), |_| false, |k| japanese.contains(&k)).map(|s| s.display())
    }

    #[test]
    fn 辞書の英単語は母音で終わっても助詞の前で分ける() {
        assert_eq!(dict_split("mergesite", &["merge"], &[]).as_deref(), Some("mergeして"));
        assert_eq!(dict_split("mergeshite", &["merge"], &[]).as_deref(), Some("mergeして"));
        assert_eq!(dict_split("mergeni", &["merge"], &[]).as_deref(), Some("mergeに"));
    }

    #[test]
    fn 辞書の英単語でも読みが日本語の語なら分けない() {
        assert_eq!(dict_split("madeni", &["made"], &["まで"]), None);
        assert_eq!(dict_split("madeni", &["made"], &[]).as_deref(), Some("madeに"));
    }

    #[test]
    fn 辞書の英単語は4文字未満なら分けない() {
        assert_eq!(dict_split("atoni", &["ato"], &[]), None);
    }

    #[test]
    fn 辞書に無い語や助詞が続かない語は分けない() {
        assert_eq!(dict_split("mergesite", &[], &[]), None);
        assert_eq!(dict_split("merger", &["merge"], &[]), None);
    }

    #[test]
    fn かなにしきれない辞書の語は読みを引かずに分ける() {
        // commit は こっみt になるので、日本語の語かは問わない。
        assert_eq!(dict_split("commitni", &["commit"], &["こっみ"]).as_deref(), Some("commitに"));
    }

    #[test]
    fn 確定済みの語も読みが日本語の語なら分けない() {
        let s = split("madeni", false, |w| w == "made", |_| false, |_| false, |_| true);
        assert_eq!(s, None);
        let s = split("donewo", false, |w| w == "done", |_| false, |_| false, |_| true);
        assert_eq!(s, None);
    }

    #[test]
    fn 確定済みの語は読みが日本語の語でなければ分ける() {
        let s = split("madeni", false, |w| w == "made", |_| false, |_| false, |_| false);
        assert_eq!(s.map(|s| s.display()).as_deref(), Some("madeに"));
        // git は ぎt になり、かなにしきれないので日本語の語になりえない。
        let s = split("gitni", false, |w| w == "git", |_| false, |_| false, |_| true);
        assert_eq!(s.map(|s| s.display()).as_deref(), Some("gitに"));
    }

    /// 辞書の語 words と、前方一致の語 prefixes を持つ語彙で分ける。
    fn relaxed(raw: &str, finished: bool, words: &[&str], prefixes: &[&str]) -> Option<String> {
        split(
            raw,
            finished,
            |_| false,
            |w| words.contains(&w),
            |w| prefixes.iter().any(|p| p.starts_with(w)),
            |_| false,
        )
        .map(|s| s.display())
    }

    #[test]
    fn 助詞で始まらない続きでも外来語らしい頭なら分ける() {
        assert_eq!(relaxed("loginjini", true, &["login"], &["login"]).as_deref(), Some("loginじに"));
        assert_eq!(relaxed("loginjinihiraku", true, &["login"], &["login"]).as_deref(), Some("loginじにひらく"));
        assert_eq!(relaxed("loginkyouha", true, &["login"], &["login"]).as_deref(), Some("loginきょうは"));
    }

    #[test]
    fn 頭に続く打ちかけのローマ字は残して表示する() {
        assert_eq!(relaxed("loginj", false, &["login"], &["login"]).as_deref(), Some("loginj"));
        assert_eq!(relaxed("loginj", true, &["login"], &["login"]), None);
    }

    #[test]
    fn かな1つ分の途中では切らない() {
        // lean|n は ん (nn) の内側。プリエディットと確定結果がずれる。
        assert_eq!(relaxed("leann", false, &["lean"], &["lean"]), None);
    }

    #[test]
    fn 確定したことのある語全体は分けない() {
        let s = split("loginji", false, |w| w == "loginji", |w| w == "login", |_| false, |_| false);
        assert_eq!(s, None);
    }

    #[test]
    fn 頭に外来の字が無ければ助詞で始まらない続きは分けない() {
        assert_eq!(relaxed("timei", false, &["time"], &["time"]), None);
        assert_eq!(relaxed("sakeiku", false, &["sake"], &["sake"]), None);
    }

    #[test]
    fn 全体が英単語の途中なら分けない() {
        assert_eq!(relaxed("informa", false, &["inform"], &["information"]), None);
    }

    #[test]
    fn 助詞で始まる分け方があればそちらを使う() {
        assert_eq!(relaxed("lineni", false, &["line", "linen"], &["line", "linen"]).as_deref(), Some("lineに"));
        assert_eq!(relaxed("linksite", false, &["link", "links"], &["link", "links"]).as_deref(), Some("linkして"));
    }

    #[test]
    fn 全体が英単語に見える打鍵列は分けない() {
        assert_eq!(relaxed("playtime", true, &["play", "time"], &["play", "time"]), None);
        assert_eq!(relaxed("lockfile", true, &["lock", "file"], &["lock", "file"]), None);
        assert_eq!(relaxed("filetype", true, &["file", "type"], &["file", "type"]), None);
    }

    #[test]
    fn 小さいかなや外来の字を含む続きは分けない() {
        // fa は ふぁ、la は ぁ になる。
        assert_eq!(relaxed("loginfa", true, &["login"], &["login"]), None);
        assert_eq!(relaxed("loginlo", true, &["login"], &["login"]), None);
        assert_eq!(relaxed("logincha", true, &["login"], &["login"]), None);
    }
}
