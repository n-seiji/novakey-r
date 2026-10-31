#![forbid(unsafe_code)]

// emoji.rs - ひらがなの読み -> 絵文字の表 (resources/emoji.tsv, Unicode CLDR 由来)。
// 表は初回参照時に一度だけ読み、ファイルが無ければ空の表として扱う。

use std::collections::HashMap;
use std::sync::LazyLock;

type Table = HashMap<String, Vec<String>>;

/// `読み\t絵文字[\t絵文字...]` の行を表にする。`#` 始まりと空行は読み飛ばす。
pub fn parse(tsv: &str) -> Table {
    tsv.lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let mut cols = l.split('\t');
            let key = cols.next()?;
            let emoji: Vec<String> = cols.filter(|e| !e.is_empty()).map(str::to_string).collect();
            (!emoji.is_empty()).then(|| (key.to_string(), emoji))
        })
        .collect()
}

/// 読みの完全一致だけで引く。
pub fn lookup<'a>(table: &'a Table, reading: &str) -> &'a [String] {
    table.get(reading).map_or(&[], Vec::as_slice)
}

static TABLE: LazyLock<Table> = LazyLock::new(|| {
    crate::config::get()
        .emoji_path
        .as_deref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| parse(&s))
        .unwrap_or_default()
});

/// ひらがなの読み全体に対応する絵文字 (表の順)。記号の呼び名 (`symbols::symbols_named`) の記号が先、絵文字が後。表に無ければ空。
pub fn emoji_for(hiragana: &str) -> Vec<String> {
    merged(crate::symbols::symbols_named(hiragana), lookup(&TABLE, hiragana))
}

/// 記号を先に、絵文字をその後ろに。同じ文字列が両方にあれば先の 1 つだけ。
fn merged(symbols: &[&str], emoji: &[String]) -> Vec<String> {
    let mut out: Vec<String> = symbols.iter().map(|s| s.to_string()).collect();
    for e in emoji {
        if !out.contains(e) {
            out.push(e.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> Table {
        parse("# header\n\nねこ\t😺\t🐱\nいぬ\t🐶\n")
    }

    #[test]
    fn コメントと空行は読み飛ばし絵文字は表の順に持つ() {
        let t = table();
        assert_eq!(t.len(), 2);
        assert_eq!(lookup(&t, "ねこ"), ["😺", "🐱"]);
    }

    #[test]
    fn 読みは完全一致だけで引く() {
        let t = table();
        assert!(lookup(&t, "ね").is_empty());
        assert!(lookup(&t, "ねこたち").is_empty());
    }

    #[test]
    fn 記号は絵文字より先に並び_重複は先の1つだけ() {
        let emoji = ["⭐".to_string(), "☆".to_string()];
        assert_eq!(merged(&["☆", "★"], &emoji), ["☆", "★", "⭐"]);
        assert_eq!(merged(&[], &emoji), ["⭐", "☆"]);
    }

    #[test]
    fn 絵文字の無い行は捨てる() {
        assert!(parse("ねこ\t\nいぬ\n").is_empty());
    }

    #[test]
    fn 同梱の表はねことびーるを引ける() {
        let tsv = include_str!("../resources/emoji.tsv");
        let t = parse(tsv);
        assert!(lookup(&t, "ねこ").contains(&"🐱".to_string()));
        assert!(lookup(&t, "びーる").contains(&"🍻".to_string()));
        // 生成器が足した U+FE0F が末尾に二重に残っていない
        assert!(t.values().flatten().all(|e| !e.ends_with("\u{fe0f}\u{fe0f}")));
        // 読みに固有な絵文字が先に来る
        assert_eq!(lookup(&t, "ねこ")[0], "🐈");
        assert_eq!(lookup(&t, "いぬ")[0], "🐕");
    }
}
