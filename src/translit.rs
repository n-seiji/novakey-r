#![forbid(unsafe_code)]

// translit.rs - 未確定文の字種変換 (ひらがな / カタカナ / 半角カタカナ / 全角英数 / 半角英数)。純関数と表だけを持つ。

/// 英数形の大文字小文字。同じキーを続けて押すと Lower → Upper → Title の順に巡る。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Case {
    /// 打鍵したままの大文字小文字。大文字を含む入力の最初の表示にだけ使う。
    Typed,
    Lower,
    Upper,
    Title,
}

impl Case {
    fn next(self) -> Self {
        match self {
            Case::Typed => Case::Lower,
            Case::Lower => Case::Upper,
            Case::Upper => Case::Title,
            Case::Title => Case::Lower,
        }
    }

    fn apply(self, text: &str) -> String {
        match self {
            Case::Typed => text.to_string(),
            Case::Lower => text.to_ascii_lowercase(),
            Case::Upper => text.to_ascii_uppercase(),
            Case::Title => {
                let mut chars = text.chars();
                chars.next().map_or_else(String::new, |head| {
                    head.to_ascii_uppercase().to_string() + &chars.as_str().to_ascii_lowercase()
                })
            }
        }
    }
}

/// 字種変換の種類。かな系は組成のひらがな、英数系は打鍵そのままの文字列を元にする。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Form {
    Hiragana,
    Katakana,
    HalfKatakana,
    FullAlnum(Case),
    HalfAlnum(Case),
}

impl Form {
    fn cycled(self) -> Self {
        match self {
            Form::FullAlnum(case) => Form::FullAlnum(case.next()),
            Form::HalfAlnum(case) => Form::HalfAlnum(case.next()),
            other => other,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Form::Hiragana => "Hiragana",
            Form::Katakana => "Katakana",
            Form::HalfKatakana => "HalfKatakana",
            Form::FullAlnum(_) => "FullAlnum",
            Form::HalfAlnum(_) => "HalfAlnum",
        }
    }
}

/// 要求された形と、いま表示中の字種変換から次の形を決める。同じ種類の英数形を続けて要求したら大文字小文字を巡らせる。
pub fn next_form(current: Option<Form>, requested: Form) -> Form {
    match current {
        Some(cur) if std::mem::discriminant(&cur) == std::mem::discriminant(&requested) => {
            cur.cycled()
        }
        _ => requested,
    }
}

/// 英数形の最初の要求は、打鍵に大文字を含むなら打鍵したままの表示から始める (Lower → Typed)。
pub fn as_typed_first(requested: Form, raw: &str) -> Form {
    if !raw.chars().any(|c| c.is_ascii_uppercase()) {
        return requested;
    }
    match requested {
        Form::FullAlnum(Case::Lower) => Form::FullAlnum(Case::Typed),
        Form::HalfAlnum(Case::Lower) => Form::HalfAlnum(Case::Typed),
        other => other,
    }
}

/// 組成のひらがな hiragana と打鍵 raw から、form の表示文字列を作る。
pub fn render(form: Form, hiragana: &str, raw: &str) -> String {
    match form {
        Form::Hiragana => hiragana.to_string(),
        Form::Katakana => to_katakana(hiragana),
        Form::HalfKatakana => to_half_katakana(hiragana),
        Form::FullAlnum(case) => to_full_width(&case.apply(raw)),
        Form::HalfAlnum(case) => case.apply(raw),
    }
}

/// ひらがなを全角カタカナへ。ひらがな以外はそのまま。
pub fn to_katakana(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\u{3041}'..='\u{3096}' | '\u{309D}'..='\u{309E}' => {
                char::from_u32(c as u32 + 0x60).unwrap_or(c)
            }
            other => other,
        })
        .collect()
}

/// 全角カタカナ(とひらがな)を半角カタカナへ。濁点・半濁点は別の文字に分ける。
pub fn to_half_katakana(text: &str) -> String {
    to_katakana(text)
        .chars()
        .map(|c| {
            HALF_KATAKANA
                .iter()
                .find(|(full, _)| *full == c)
                .map_or_else(|| c.to_string(), |(_, half)| (*half).to_string())
        })
        .collect()
}

/// ASCII の印字可能文字と空白を全角へ。それ以外はそのまま。
pub fn to_full_width(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            ' ' => '\u{3000}',
            '!'..='~' => char::from_u32(c as u32 + 0xFEE0).unwrap_or(c),
            other => other,
        })
        .collect()
}

/// (全角カタカナ・記号, 半角)。ヴや濁音は 2 文字になる。
const HALF_KATAKANA: &[(char, &str)] = &[
    ('ァ', "ｧ"), ('ア', "ｱ"), ('ィ', "ｨ"), ('イ', "ｲ"), ('ゥ', "ｩ"), ('ウ', "ｳ"),
    ('ェ', "ｪ"), ('エ', "ｴ"), ('ォ', "ｫ"), ('オ', "ｵ"),
    ('カ', "ｶ"), ('ガ', "ｶﾞ"), ('キ', "ｷ"), ('ギ', "ｷﾞ"), ('ク', "ｸ"), ('グ', "ｸﾞ"),
    ('ケ', "ｹ"), ('ゲ', "ｹﾞ"), ('コ', "ｺ"), ('ゴ', "ｺﾞ"), ('ヵ', "ｶ"), ('ヶ', "ｹ"),
    ('サ', "ｻ"), ('ザ', "ｻﾞ"), ('シ', "ｼ"), ('ジ', "ｼﾞ"), ('ス', "ｽ"), ('ズ', "ｽﾞ"),
    ('セ', "ｾ"), ('ゼ', "ｾﾞ"), ('ソ', "ｿ"), ('ゾ', "ｿﾞ"),
    ('タ', "ﾀ"), ('ダ', "ﾀﾞ"), ('チ', "ﾁ"), ('ヂ', "ﾁﾞ"), ('ッ', "ｯ"), ('ツ', "ﾂ"), ('ヅ', "ﾂﾞ"),
    ('テ', "ﾃ"), ('デ', "ﾃﾞ"), ('ト', "ﾄ"), ('ド', "ﾄﾞ"),
    ('ナ', "ﾅ"), ('ニ', "ﾆ"), ('ヌ', "ﾇ"), ('ネ', "ﾈ"), ('ノ', "ﾉ"),
    ('ハ', "ﾊ"), ('バ', "ﾊﾞ"), ('パ', "ﾊﾟ"), ('ヒ', "ﾋ"), ('ビ', "ﾋﾞ"), ('ピ', "ﾋﾟ"),
    ('フ', "ﾌ"), ('ブ', "ﾌﾞ"), ('プ', "ﾌﾟ"), ('ヘ', "ﾍ"), ('ベ', "ﾍﾞ"), ('ペ', "ﾍﾟ"),
    ('ホ', "ﾎ"), ('ボ', "ﾎﾞ"), ('ポ', "ﾎﾟ"),
    ('マ', "ﾏ"), ('ミ', "ﾐ"), ('ム', "ﾑ"), ('メ', "ﾒ"), ('モ', "ﾓ"),
    ('ャ', "ｬ"), ('ヤ', "ﾔ"), ('ュ', "ｭ"), ('ユ', "ﾕ"), ('ョ', "ｮ"), ('ヨ', "ﾖ"),
    ('ラ', "ﾗ"), ('リ', "ﾘ"), ('ル', "ﾙ"), ('レ', "ﾚ"), ('ロ', "ﾛ"),
    ('ヮ', "ﾜ"), ('ワ', "ﾜ"), ('ヲ', "ｦ"), ('ン', "ﾝ"), ('ヴ', "ｳﾞ"),
    ('ー', "ｰ"), ('。', "｡"), ('、', "､"), ('「', "｢"), ('」', "｣"), ('・', "･"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ひらがなを全角カタカナへ_かな以外は変えない() {
        for (input, want) in [
            ("きょう", "キョウ"),
            ("ゔぁ", "ヴァ"),
            ("ぱっと", "パット"),
            ("らーめん", "ラーメン"),
            ("ゝゞ", "ヽヾ"),
            ("「あ」。", "「ア」。"),
            ("えんてr", "エンテr"),
            ("カタカナ漢字1", "カタカナ漢字1"),
        ] {
            assert_eq!(to_katakana(input), want, "{input}");
        }
    }

    #[test]
    fn 半角カタカナは濁点と半濁点を分け_長音と小書きと記号を対応させる() {
        for (input, want) in [
            ("がぎぐげご", "ｶﾞｷﾞｸﾞｹﾞｺﾞ"),
            ("ぱぴぷぺぽ", "ﾊﾟﾋﾟﾌﾟﾍﾟﾎﾟ"),
            ("ゔ", "ｳﾞ"),
            ("っ", "ｯ"),
            ("ゃゅょぁぃぅぇぉ", "ｬｭｮｧｨｩｪｫ"),
            ("らーめん", "ﾗｰﾒﾝ"),
            ("「あ」、。・", "｢ｱ｣､｡･"),
            ("じゅうしょ", "ｼﾞｭｳｼｮ"),
            ("えんてr", "ｴﾝﾃr"),
            ("漢字〜", "漢字〜"),
        ] {
            assert_eq!(to_half_katakana(input), want, "{input}");
        }
    }

    #[test]
    fn 全角変換はasciiの印字可能文字と空白だけを動かす() {
        assert_eq!(to_full_width("enter"), "ｅｎｔｅｒ");
        assert_eq!(to_full_width("A-z 09~!"), "Ａ－ｚ　０９～！");
        assert_eq!(to_full_width("あ"), "あ");
    }

    #[test]
    fn かな形は組成のひらがな_英数形は打鍵から作る() {
        assert_eq!(render(Form::Hiragana, "えんてr", "enter"), "えんてr");
        assert_eq!(render(Form::Katakana, "えんてr", "enter"), "エンテr");
        assert_eq!(render(Form::HalfKatakana, "えんてr", "enter"), "ｴﾝﾃr");
        assert_eq!(render(Form::FullAlnum(Case::Lower), "えんてr", "enter"), "ｅｎｔｅｒ");
        assert_eq!(render(Form::HalfAlnum(Case::Lower), "えんてr", "enter"), "enter");
    }

    #[test]
    fn 英数形は同じ種類を続けて要求すると大文字小文字を巡る() {
        let half = Form::HalfAlnum(Case::Lower);
        let step = |form: Form| next_form(Some(form), half);
        assert_eq!(next_form(None, half), Form::HalfAlnum(Case::Lower));
        assert_eq!(step(Form::HalfAlnum(Case::Lower)), Form::HalfAlnum(Case::Upper));
        assert_eq!(step(Form::HalfAlnum(Case::Upper)), Form::HalfAlnum(Case::Title));
        assert_eq!(step(Form::HalfAlnum(Case::Title)), Form::HalfAlnum(Case::Lower));
        assert_eq!(render(Form::HalfAlnum(Case::Title), "", "eNTER"), "Enter");
        assert_eq!(render(Form::FullAlnum(Case::Upper), "", "ab"), "ＡＢ");
    }

    #[test]
    fn 別の種類を要求すれば巡らず要求どおりになる() {
        assert_eq!(
            next_form(Some(Form::HalfAlnum(Case::Upper)), Form::FullAlnum(Case::Lower)),
            Form::FullAlnum(Case::Lower)
        );
        assert_eq!(next_form(Some(Form::Katakana), Form::Katakana), Form::Katakana);
        assert_eq!(next_form(Some(Form::Katakana), Form::Hiragana), Form::Hiragana);
    }
}
