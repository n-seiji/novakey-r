#![forbid(unsafe_code)]

// english_split.rs - 英単語のあとに助詞などが続く打鍵列 (artifactni) を、英単語とかなに分ける。
// かなの単位に頼らず打鍵列を切り直す (artifactwo の two は とぉ ではなく t + を)。

use crate::romaji_converter::{Flush, RomajiConverter};

/// 英単語の直後に来てよいかなの始まり。include の de や partner の ne を切らないよう、よく使う助詞などに限る。
const TAILS: &[&str] = &[
    "に", "を", "が", "は", "で", "と", "の", "も", "へ", "から", "まで", "より", "って", "する", "して",
    "した", "しま", "され", "させ", "でき",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Split {
    pub head: String,
    /// かなの部分の (打鍵, かな) の組。
    pub tail: Vec<(String, String)>,
    /// かなの部分の未確定のローマ字。
    pub pending: String,
}

impl Split {
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
    if finished {
        match conv.flush() {
            Flush::Empty => {}
            Flush::Kana(kana) | Flush::Dangling(kana) => units.push((std::mem::take(&mut typed), kana)),
        }
    }
    (units, conv.buffer().to_string())
}

fn has_ascii(units: &[(String, String)]) -> bool {
    units.iter().any(|(_, kana)| kana.chars().any(|c| c.is_ascii_alphabetic()))
}

/// raw を 英単語 + 助詞などで始まるかな に分ける。分けられなければ None。
/// 英単語と見なすのは、全体がかなにしきれない (artifact の rt) か、英語として確定したことのある語。
/// finished は Enter などで打ち終えたとき。途中なら末尾の未確定ローマ字を許す。
pub fn split(raw: &str, finished: bool, knows: impl Fn(&str) -> bool) -> Option<Split> {
    if raw.len() < 3 || !raw.bytes().all(|b| b.is_ascii_lowercase()) {
        return None;
    }
    let (whole, _) = convert(raw, true);
    let foreign = has_ascii(&whole);
    (2..raw.len()).rev().find_map(|i| {
        let (head, rest) = raw.split_at(i);
        let consonant_end = !head.ends_with(['a', 'i', 'u', 'e', 'o']);
        if !(consonant_end && foreign || knows(head)) {
            return None;
        }
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(raw: &str) -> Option<String> {
        split(raw, false, |_| false).map(|s| s.display())
    }

    #[test]
    fn 英単語のあとの助詞をかなにする() {
        assert_eq!(shown("artifactni").as_deref(), Some("artifactに"));
        assert_eq!(shown("artifactwotsukau").as_deref(), Some("artifactをつかう"));
        assert_eq!(shown("artifactnisuru").as_deref(), Some("artifactにする"));
    }

    #[test]
    fn 途中のローマ字は残して表示する() {
        assert_eq!(shown("artifactnis").as_deref(), Some("artifactにs"));
    }

    #[test]
    fn 打ち終えたら残りのローマ字が変換できないものは分けない() {
        assert_eq!(split("artifactnish", true, |_| false), None);
        let done = split("artifactnin", true, |_| false).unwrap();
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
        assert_eq!(split("mergeni", false, |w| w == "merge").map(|s| s.display()).as_deref(), Some("mergeに"));
        assert_eq!(split("openni", false, |w| w == "open").map(|s| s.display()).as_deref(), Some("openに"));
        assert_eq!(split("openni", false, |_| false), None);
    }
}
