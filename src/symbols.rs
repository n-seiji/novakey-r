#![forbid(unsafe_code)]

// symbols.rs - 記号の表。打鍵した ASCII の既定の和文表記と、スペースで選べる異体の候補。

/// (打鍵した ASCII, 既定の和文表記)。数字はここに含めない。
const DEFAULT_FORMS: &[(&str, &str)] = &[
    (".", "。"),
    (",", "、"),
    ("-", "ー"),
    ("!", "！"),
    ("?", "？"),
    ("[", "「"),
    ("]", "」"),
    ("(", "（"),
    (")", "）"),
    ("~", "〜"),
    ("/", "・"),
    ("{", "｛"),
    ("}", "｝"),
    (":", "："),
    (";", "；"),
];

/// 先頭が既定の表記、続きが異体。記号だけの組成でスペースを押すと、この行が候補になる。
const VARIANTS: &[&[&str]] = &[
    &["「", "『", "【", "〔", "［", "〈", "《", "(", "（", "｢"],
    &["」", "』", "】", "〕", "］", "〉", "》", ")", "）", "｣"],
    &["（", "(", "【", "「", "『", "〔", "［"],
    &["）", ")", "】", "」", "』", "〕", "］"],
    &["。", "．", "."],
    &["、", "，", ","],
    &["ー", "－", "-", "‐", "—", "～"],
    &["！", "!"],
    &["？", "?"],
    &["〜", "～", "~"],
    &["・", "／", "/"],
    &["｛", "{"],
    &["｝", "}"],
    &["：", ":"],
    &["；", ";"],
];

/// 記号の呼び名 (ひらがなの読み全体) -> 記号。絵文字と同じ経路 (`emoji::emoji_for` -> composer の絵文字候補) で、
/// 表の順に絵文字より先に出る。エンジンの辞書にも居る記号は、後ろの複製が消えてここの位置に寄る。手で足す表。
const NAMES: &[(&str, &[&str])] = &[
    ("こめじるし", &["※"]),
    ("まる", &["○", "●", "◎"]),
    ("しろまる", &["○"]),
    ("くろまる", &["●"]),
    ("にじゅうまる", &["◎"]),
    ("さんかく", &["△", "▲", "▽", "▼"]),
    ("しろさんかく", &["△"]),
    ("くろさんかく", &["▲"]),
    ("しかく", &["□", "■"]),
    ("しろしかく", &["□"]),
    ("くろしかく", &["■"]),
    ("ほし", &["☆", "★"]),
    ("しろほし", &["☆"]),
    ("くろほし", &["★"]),
    ("やじるし", &["→", "←", "↑", "↓"]),
    ("ゆうびん", &["〒"]),
    ("おんぷ", &["♪", "♫", "♬"]),
    ("ばつ", &["×"]),
    ("ひしがた", &["◇", "◆"]),
    ("でんわ", &["☎"]),
    ("かっこ", &["「」", "『』", "（）", "【】"]),
    ("なみ", &["〜", "～"]),
];

/// 記号の呼び名に対応する記号 (表の順)。読み全体の完全一致だけ。無ければ空。
pub fn symbols_named(reading: &str) -> &'static [&'static str] {
    NAMES.iter().find(|(name, _)| *name == reading).map_or(&[], |(_, symbols)| symbols)
}

/// 打鍵した ASCII 記号の既定の和文表記。
pub fn default_form(ascii: &str) -> Option<&'static str> {
    DEFAULT_FORMS
        .iter()
        .find(|(key, _)| *key == ascii)
        .map(|(_, form)| *form)
}

/// 和文表記 form の候補一覧(先頭が form 自身)。
pub fn variants(form: &str) -> Option<&'static [&'static str]> {
    VARIANTS.iter().find(|row| row[0] == form).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 打鍵した記号は既定の和文表記になる() {
        let expected = [
            (".", "。"), (",", "、"), ("-", "ー"), ("!", "！"), ("?", "？"), ("[", "「"),
            ("]", "」"), ("(", "（"), (")", "）"), ("~", "〜"), ("/", "・"), ("{", "｛"),
            ("}", "｝"), (":", "："), (";", "；"),
        ];
        for (ascii, form) in expected {
            assert_eq!(default_form(ascii), Some(form), "{ascii}");
        }
        assert_eq!(default_form("a"), None);
        assert_eq!(default_form("1"), None);
    }

    #[test]
    fn 記号の呼び名は読み全体の完全一致で引ける() {
        assert_eq!(symbols_named("こめじるし"), ["※"]);
        assert_eq!(symbols_named("まる"), ["○", "●", "◎"]);
        assert!(symbols_named("こめ").is_empty());
        assert!(symbols_named("まるい").is_empty());
    }

    #[test]
    fn 記号の呼び名の表は空の読みも空の記号も重複した読みも持たない() {
        for (i, (name, symbols)) in NAMES.iter().enumerate() {
            assert!(!name.is_empty() && !symbols.is_empty() && symbols.iter().all(|s| !s.is_empty()), "{name}");
            assert!(NAMES[..i].iter().all(|(n, _)| n != name), "{name} が重複");
        }
    }

    #[test]
    fn すべての既定表記に異体の行があり_先頭は既定表記() {
        for (ascii, form) in DEFAULT_FORMS {
            let row = variants(form).unwrap_or_else(|| panic!("{ascii} の行が無い"));
            assert_eq!(row[0], *form);
        }
    }

    #[test]
    fn 括弧の異体は指定どおりの順() {
        assert_eq!(
            variants("「"),
            Some(&["「", "『", "【", "〔", "［", "〈", "《", "(", "（", "｢"][..])
        );
        assert_eq!(variants("。"), Some(&["。", "．", "."][..]));
        assert_eq!(variants("あ"), None);
    }
}
