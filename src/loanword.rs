#![forbid(unsafe_code)]

// loanword.rs - カタカナ語 -> 英単語の表 (resources/loanwords.tsv, JMdict 由来)。
// 表は初回参照時に一度だけ読み、ファイルが無ければ空の表として扱う。

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

type Table = HashMap<String, Vec<String>>;

/// `カタカナ\t英単語[\t英単語...]` の行を表にする。`#` 始まりと空行は読み飛ばす。
pub fn parse(tsv: &str) -> Table {
    tsv.lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let mut cols = l.split('\t');
            let key = cols.next()?;
            let words: Vec<String> = cols.filter(|w| !w.is_empty()).map(str::to_string).collect();
            (!words.is_empty()).then(|| (key.to_string(), words))
        })
        .collect()
}

/// 完全一致を優先し、無ければ末尾の長音 (サーバ <-> サーバー) を足し引きして引く。
pub fn lookup<'a>(table: &'a Table, katakana: &str) -> &'a [String] {
    let variant = match katakana.strip_suffix('ー') {
        Some(stem) => stem.to_string(),
        None => format!("{katakana}ー"),
    };
    let found = table.get(katakana).or_else(|| table.get(&variant));
    found.map_or(&[], Vec::as_slice)
}

static TABLE: LazyLock<Table> = LazyLock::new(|| {
    crate::config::get()
        .loanwords_path
        .as_deref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| parse(&s))
        .unwrap_or_default()
});

/// カタカナ語に対応する英単語 (最大 3 件)。表に無ければ空。
pub fn english_for(katakana: &str) -> Vec<String> {
    lookup(&TABLE, katakana).to_vec()
}

/// 表の英語 (open account のような句は語に分ける) のうち、小文字の英字だけの語の集合。
pub fn words(table: &Table) -> HashSet<&str> {
    table
        .values()
        .flatten()
        .flat_map(|w| w.split(' '))
        .filter(|w| w.bytes().all(|b| b.is_ascii_lowercase()))
        .collect()
}

/// 1 行 1 語の一覧 (`resources/dev_words.txt`)。`#` 始まりと空行は読み飛ばし、小文字の英字だけの行を語とする。
pub fn parse_words(text: &str) -> HashSet<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter(|w| w.bytes().all(|b| b.is_ascii_lowercase()))
        .map(str::to_string)
        .collect()
}

/// 表の英語に、開発用語の一覧 (JMdict に載らない merge, commit など) を足した集合。一覧が無ければ表だけ。
static WORDS: LazyLock<HashSet<String>> = LazyLock::new(|| {
    let extra = crate::config::get()
        .dev_words_path
        .as_deref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| parse_words(&s))
        .unwrap_or_default();
    words(&TABLE).into_iter().map(str::to_string).chain(extra).collect()
});

/// 打鍵列が表に載っている英単語か。
pub fn is_english_word(typed: &str) -> bool {
    WORDS.contains(typed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> Table {
        parse("# header\n\nサーバー\tserver\nライト\tlight\tright\nデスクトップ\tdesktop\n")
    }

    #[test]
    fn コメントと空行は読み飛ばし複数の語は順に持つ() {
        let t = table();
        assert_eq!(t.len(), 3);
        assert_eq!(lookup(&t, "ライト"), ["light", "right"]);
    }

    #[test]
    fn 完全一致で引ける() {
        assert_eq!(lookup(&table(), "デスクトップ"), ["desktop"]);
    }

    #[test]
    fn 表に無い語は空() {
        assert!(lookup(&table(), "ウナギ").is_empty());
    }

    #[test]
    fn 末尾の長音は有無どちらでも引ける() {
        let t = table();
        assert_eq!(lookup(&t, "サーバ"), ["server"]);
        assert_eq!(lookup(&t, "ライトー"), ["light", "right"]);
    }

    #[test]
    fn 英単語の集合は句を語に分け小文字の英字だけの語を持つ() {
        let t = parse("ア\tenter\nイ\topen account\tiPhone\tT-shirt\n");
        assert_eq!(words(&t), HashSet::from(["enter", "open", "account"]));
    }

    #[test]
    fn 一覧はコメントと空行を読み飛ばし小文字の英字だけの語を持つ() {
        let w = parse_words("# dev\n\nmerge\n  commit \nOAuth\nT-shirt\n");
        assert_eq!(w, HashSet::from(["merge".to_string(), "commit".to_string()]));
    }

    #[test]
    fn 語の無い行は捨てる() {
        assert!(parse("ア\t\nイ\n").is_empty());
    }
}
