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

/// 1 行 1 語の一覧 (`resources/dev_words.txt`)。`#` 始まりと空行は読み飛ばし、小文字の英字で始まり小文字の英字と数字だけの行を語とする (base64, md5)。
pub fn parse_words(text: &str) -> HashSet<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter(|w| w.bytes().next().is_some_and(|b| b.is_ascii_lowercase()) && w.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()))
        .map(str::to_string)
        .collect()
}

/// 開発用語の一覧 (`resources/dev_words.txt`)。一覧が無ければ空。
static DEV_WORDS: LazyLock<HashSet<String>> = LazyLock::new(|| {
    crate::config::get()
        .dev_words_path
        .as_deref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| parse_words(&s))
        .unwrap_or_default()
});

/// 表の英語に、開発用語の一覧 (JMdict に載らない merge, commit など) を足した語の一覧。一覧が無ければ表だけ。
/// 数字を含む語 (base64) は入れない: 英語分割 (`english_split`) や前方一致は英字だけの語を前提にしていて、数字付きの語は `is_dev_word` / `is_dev_word_prefix` だけで引く。
/// 完全一致 (binary_search) と前方一致 (partition_point) の両方を引けるよう、整列して重複を除く。
static WORDS: LazyLock<Vec<String>> = LazyLock::new(|| {
    let mut all: Vec<String> = words(&TABLE).into_iter().map(str::to_string).chain(DEV_WORDS.iter().filter(|w| w.bytes().all(|b| b.is_ascii_lowercase())).cloned()).collect();
    all.sort_unstable();
    all.dedup();
    all
});

/// 打鍵列が表に載っている英単語か。
pub fn is_english_word(typed: &str) -> bool {
    WORDS.binary_search_by(|w| w.as_str().cmp(typed)).is_ok()
}

/// 打鍵列が開発用語の一覧 (`dev_words.txt`) の語か。表の語と違い、短くても英語と見なしてよい語だけが載る。
pub fn is_dev_word(typed: &str) -> bool {
    DEV_WORDS.contains(typed)
}

/// 打鍵列で始まる開発用語の一覧の語があるか (打鍵列そのものが語の場合も含む)。数字を含む語 (base64) の途中 (base6) を見分けるのに使う。
pub fn is_dev_word_prefix(typed: &str) -> bool {
    DEV_WORDS.iter().any(|w| w.starts_with(typed))
}

/// 打鍵列で始まる英単語があるか (打鍵列そのものが語の場合も含む)。
pub fn is_english_prefix(typed: &str) -> bool {
    has_prefix(&WORDS, typed)
}

/// 整列済みの語の一覧に、typed で始まる語があるか。
fn has_prefix(sorted: &[String], typed: &str) -> bool {
    let i = sorted.partition_point(|w| w.as_str() < typed);
    sorted.get(i).is_some_and(|w| w.starts_with(typed))
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
    fn 一覧は英字で始まる数字付きの語を持ち数字で始まる行は捨てる() {
        let w = parse_words("base64\nmd5\n64bit\n123\nbase-64\n");
        assert_eq!(w, HashSet::from(["base64".to_string(), "md5".to_string()]));
    }

    #[test]
    fn 語の無い行は捨てる() {
        assert!(parse("ア\t\nイ\n").is_empty());
    }

    #[test]
    fn 前方一致は語そのものと語の途中を見つけ無関係な打鍵列は見つけない() {
        let sorted: Vec<String> = ["inform", "information", "login"].iter().map(|w| w.to_string()).collect();
        assert!(has_prefix(&sorted, "informa"));
        assert!(has_prefix(&sorted, "login"));
        assert!(has_prefix(&sorted, "log"));
        assert!(!has_prefix(&sorted, "logini"));
        assert!(!has_prefix(&sorted, "zzz"));
    }
}
