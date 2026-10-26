//! live 変換の Backspace が消す読みの長さを決める (表示中の最後の 1 文字に対応する読み)。
#![forbid(unsafe_code)]

use super::dict::to_katakana;

/// 漢字 (々 を含む)。
fn is_kanji(c: char) -> bool {
    matches!(c as u32, 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x3005)
}

/// 読みの先頭に来られない文字 (小書きのかな・長音・ン・ッ)。
fn cannot_start_reading(c: char) -> bool {
    matches!(c, 'ァ' | 'ィ' | 'ゥ' | 'ェ' | 'ォ' | 'ッ' | 'ャ' | 'ュ' | 'ョ' | 'ヮ' | 'ヵ' | 'ヶ' | 'ー' | 'ン')
}

/// 候補の語列 `words` ((表記, カタカナの読み)) から、最後の表示文字に当たるひらがな (= カタカナ化後も同じ) の文字数を返す。
/// 語列が読みと表示に一致しない (学習・補正・先頭文節などで組み立てが違う) ときと、規則に当てはまらないときは None。
/// `has_entry(読みの末尾, 漢字)` は、その読みでその 1 字が辞書にあるか。
pub fn drop_len(
    words: &[(&str, &str)],
    hiragana: &str,
    displayed: &str,
    has_entry: impl Fn(&str, char) -> bool,
) -> Option<usize> {
    let ruby_all: String = words.iter().map(|(_, ruby)| *ruby).collect();
    let word_all: String = words.iter().flat_map(|(word, _)| word.chars()).filter(|&c| c != '\u{1F}').collect();
    if ruby_all != to_katakana(hiragana) || word_all != displayed {
        return None;
    }
    let (word, ruby) = words.last()?;
    let word: Vec<char> = word.chars().filter(|&c| c != '\u{1F}').collect();
    let ruby: Vec<char> = ruby.chars().collect();
    let (&last_word, &last_ruby) = (word.last()?, ruby.last()?);
    if to_katakana(&last_word.to_string()) == last_ruby.to_string() {
        return Some(1);
    }
    if word.len() == 1 {
        return Some(ruby.len());
    }
    if !is_kanji(last_word) {
        return None;
    }
    let run = word.iter().rev().take_while(|&&c| is_kanji(c)).count();
    // 語の残りの 1 字につき読みを 1 文字は残す。
    for len in (1..=ruby.len().saturating_sub(word.len() - 1)).rev() {
        let suffix = &ruby[ruby.len() - len..];
        if cannot_start_reading(suffix[0]) {
            continue;
        }
        if has_entry(&suffix.iter().collect::<String>(), last_word) {
            return Some(len);
        }
    }
    // 漢字 1 字の前にかなが付く語 (お金・の件) は、切り出せなければ語全体を消さず 1 文字だけにする。
    (run >= 2).then_some(ruby.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(_: &str, _: char) -> bool {
        false
    }

    #[test]
    fn 一字の語は読み全体を消す() {
        assert_eq!(drop_len(&[("その", "ソノ"), ("件", "ケン")], "そのけん", "その件", none), Some(2));
    }

    #[test]
    fn 送り仮名など末尾が読みと同じなら1字だけ消す() {
        assert_eq!(drop_len(&[("食べる", "タベル")], "たべる", "食べる", none), Some(1));
        assert_eq!(drop_len(&[("お願いします", "オネガイシマス")], "おねがいします", "お願いします", none), Some(1));
    }

    #[test]
    fn カタカナ表記は1字だけ消す() {
        assert_eq!(drop_len(&[("コーヒー", "コーヒー")], "こーひー", "コーヒー", none), Some(1));
    }

    #[test]
    fn 数字や英字が読みと同じなら1字だけ消す() {
        assert_eq!(drop_len(&[("123", "123")], "123", "123", none), Some(1));
    }

    #[test]
    fn 複数漢字は辞書にある最長の読みで最後の漢字だけ消す() {
        let has = |suffix: &str, k: char| matches!((suffix, k), ("シャ", '社') | ("セイ", '生') | ("カン", '間'));
        assert_eq!(drop_len(&[("会社", "カイシャ")], "かいしゃ", "会社", has), Some(2));
        assert_eq!(drop_len(&[("先生", "センセイ")], "せんせい", "先生", has), Some(2));
        assert_eq!(drop_len(&[("時間", "ジカン")], "じかん", "時間", has), Some(2));
    }

    #[test]
    fn 読みの先頭になれない文字からの切り出しは試さない() {
        let calls = std::cell::RefCell::new(Vec::new());
        let has = |suffix: &str, k: char| {
            calls.borrow_mut().push((suffix.to_string(), k));
            suffix == "セイ"
        };
        assert_eq!(drop_len(&[("先生", "センセイ")], "せんせい", "先生", has), Some(2));
        assert!(calls.borrow().iter().all(|(s, _)| s != "ンセイ" && s != "ャ"));
    }

    #[test]
    fn 切り出せない複数漢字は語全体を消す() {
        assert_eq!(drop_len(&[("今日", "キョウ")], "きょう", "今日", none), Some(3));
    }

    #[test]
    fn 残りの漢字に読みを残す() {
        let seen = std::cell::RefCell::new(Vec::new());
        let has = |suffix: &str, _k: char| {
            seen.borrow_mut().push(suffix.to_string());
            false
        };
        assert_eq!(drop_len(&[("今日", "キョウ")], "きょう", "今日", has), Some(3));
        // ョウ は小書きで始まるので試さず、ウ だけを引く。
        assert_eq!(seen.borrow().as_slice(), ["ウ"]);
    }

    #[test]
    fn 末尾が記号で読みと違うなら当てはまらない() {
        assert_eq!(drop_len(&[("です!", "デス！")], "です！", "です!", none), None);
    }

    #[test]
    fn かなの後ろの漢字1字は辞書にある読みだけ消す() {
        let has = |suffix: &str, k: char| matches!((suffix, k), ("ケン", '件') | ("カネ", '金'));
        assert_eq!(drop_len(&[("会社", "カイシャ"), ("の件", "ノケン")], "かいしゃのけん", "会社の件", has), Some(2));
        assert_eq!(drop_len(&[("お金", "オカネ")], "おかね", "お金", has), Some(2));
    }

    #[test]
    fn かなの後ろの漢字1字が切り出せなければ当てはまらない() {
        assert_eq!(drop_len(&[("お金", "オカネ")], "おかね", "お金", none), None);
    }

    #[test]
    fn 語列が読みと合わなければ当てはまらない() {
        assert_eq!(drop_len(&[("件", "ケン")], "そのけん", "その件", none), None);
    }

    #[test]
    fn 語列が表示と合わなければ当てはまらない() {
        assert_eq!(drop_len(&[("その", "ソノ"), ("件", "ケン")], "そのけん", "その毛", none), None);
    }

    #[test]
    fn 内部の区切り文字は表示から除いて比べる() {
        assert_eq!(drop_len(&[("その", "ソノ"), ("\u{1F}件", "ケン")], "そのけん", "その件", none), Some(2));
    }

    #[test]
    fn 語が無ければ当てはまらない() {
        assert_eq!(drop_len(&[], "", "", none), None);
    }
}
