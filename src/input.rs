#![forbid(unsafe_code)]
//! Cocoa を知らないキー入力の表現と、その振り分け。imk.rs (実機) と simulate.rs (headless) が共有する。
use crate::keymap::{self, Key};
use crate::translit::Form;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub control: bool,
    /// Command / Option のいずれかが押されている。
    pub command_or_option: bool,
}

/// keyDown 1 打鍵分の値。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyInput {
    pub key_code: u16,
    pub characters: String,
    /// Shift 以外の修飾キーを無視した文字 (Ctrl 押下中にどのキーかを読む)。
    pub characters_ignoring_modifiers: String,
    pub mods: Modifiers,
}

#[derive(Clone, PartialEq, Eq)]
pub enum Route {
    /// Command / Option、字種変換キーでない Ctrl。IME は触らずアプリへ渡す。
    PassThrough,
    Translit(Form),
    /// どれか 1 つでも処理したら全体を処理済みとして扱う (未処理キーの文字は捨てる)。
    Keys(Vec<(Key, String)>),
    /// ←→ や機能キー。未確定文があれば握りつぶし、無ければアプリへ素通しする (判定は呼び出し側)。
    Navigation,
}

// Keys が入力文字列を持つので、derive(Debug) だとログ/trace に打鍵が漏れる。文字列は出さず Key の名前だけにする。
impl std::fmt::Debug for Route {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Route::PassThrough => f.write_str("PassThrough"),
            Route::Translit(form) => f.debug_tuple("Translit").field(form).finish(),
            Route::Keys(keys) => f.debug_tuple("Keys").field(&keys.iter().map(|(k, _)| k.name()).collect::<Vec<_>>()).finish(),
            Route::Navigation => f.write_str("Navigation"),
        }
    }
}

pub fn route(input: &KeyInput) -> Route {
    let translit = (!input.mods.command_or_option)
        .then(|| {
            keymap::translit_form(
                input.key_code,
                input.mods.control,
                &input.characters_ignoring_modifiers,
            )
        })
        .flatten();
    if translit.is_none() && (input.mods.control || input.mods.command_or_option) {
        return Route::PassThrough;
    }
    if let Some(form) = translit {
        return Route::Translit(form);
    }
    stroke(input.key_code, &input.characters, input.mods.shift)
}

/// keyCode と characters から打鍵を解釈する。特殊キーは keyCode で、それ以外は文字で分類する。
fn stroke(key_code: u16, characters: &str, shift: bool) -> Route {
    let special = match key_code {
        36 | 76 => Some(Key::Enter),
        53 => Some(Key::Escape),
        51 => Some(Key::Backspace),
        48 => Some(Key::Control),
        49 if shift => Some(Key::ShiftSpace),
        49 => Some(Key::Space),
        125 => Some(Key::Down),
        126 => Some(Key::Up),
        123 | 124 => return Route::Navigation,
        _ => None,
    };
    if let Some(key) = special {
        return Route::Keys(vec![(key, String::new())]);
    }
    let is_function_key = |c: char| ('\u{F700}'..='\u{F8FF}').contains(&c);
    if characters.is_empty() || characters.chars().any(is_function_key) {
        return Route::Navigation;
    }
    Route::Keys(
        characters
            .chars()
            .map(|ch| (keymap::classify(ch), ch.to_string()))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::translit::Case;

    fn input(key_code: u16, chars: &str, mods: Modifiers) -> KeyInput {
        KeyInput {
            key_code,
            characters: chars.to_string(),
            characters_ignoring_modifiers: chars.to_string(),
            mods,
        }
    }

    const NONE: Modifiers = Modifiers { shift: false, control: false, command_or_option: false };

    #[test]
    fn command_or_option_pass_through_even_for_translit_keys() {
        let m = Modifiers { command_or_option: true, ..NONE };
        assert_eq!(route(&input(0, "a", m)), Route::PassThrough);
        assert_eq!(route(&input(97, "\u{F709}", m)), Route::PassThrough);
        let both = Modifiers { control: true, command_or_option: true, ..NONE };
        assert_eq!(route(&input(0, "k", both)), Route::PassThrough);
    }

    #[test]
    fn control_translit_keys_become_translit() {
        let m = Modifiers { control: true, ..NONE };
        assert_eq!(route(&input(0, "k", m)), Route::Translit(Form::Katakana));
        assert_eq!(route(&input(0, "K", m)), Route::Translit(Form::Katakana));
    }

    #[test]
    fn control_reads_characters_ignoring_modifiers() {
        let m = Modifiers { control: true, ..NONE };
        // 実機では Ctrl+K の characters は制御文字 "\u{b}" になる。
        let ctrl_k = KeyInput {
            key_code: 40,
            characters: "\u{b}".to_string(),
            characters_ignoring_modifiers: "k".to_string(),
            mods: m,
        };
        assert_eq!(route(&ctrl_k), Route::Translit(Form::Katakana));
        // characters 側が k でも、判定は ignoring 側 (a) で行われる。
        let swapped = KeyInput {
            key_code: 0,
            characters: "k".to_string(),
            characters_ignoring_modifiers: "a".to_string(),
            mods: m,
        };
        assert_eq!(route(&swapped), Route::PassThrough);
    }

    #[test]
    fn control_other_keys_pass_through() {
        let m = Modifiers { control: true, ..NONE };
        assert_eq!(route(&input(0, "a", m)), Route::PassThrough);
        assert_eq!(route(&input(36, "\r", m)), Route::PassThrough);
    }

    #[test]
    fn function_keys_f6_to_f10_become_translit() {
        let lower = Case::Lower;
        for (code, ch, want) in [
            (97, "\u{F709}", Form::Hiragana),
            (98, "\u{F70A}", Form::Katakana),
            (100, "\u{F70B}", Form::HalfKatakana),
            (101, "\u{F70C}", Form::FullAlnum(lower)),
            (109, "\u{F70D}", Form::HalfAlnum(lower)),
        ] {
            assert_eq!(route(&input(code, ch, NONE)), Route::Translit(want), "key_code {code}");
        }
    }

    #[test]
    fn left_and_right_arrows_are_navigation() {
        assert_eq!(route(&input(123, "\u{F702}", NONE)), Route::Navigation);
        assert_eq!(route(&input(124, "\u{F703}", NONE)), Route::Navigation);
    }

    #[test]
    fn other_function_keys_and_empty_characters_are_navigation() {
        assert_eq!(route(&input(115, "\u{F729}", NONE)), Route::Navigation);
        assert_eq!(route(&input(0, "", NONE)), Route::Navigation);
    }

    #[test]
    fn up_down_and_special_keys_are_keys() {
        assert_eq!(route(&input(126, "\u{F700}", NONE)), Route::Keys(vec![(Key::Up, String::new())]));
        assert_eq!(route(&input(125, "\u{F701}", NONE)), Route::Keys(vec![(Key::Down, String::new())]));
        assert_eq!(route(&input(36, "\r", NONE)), Route::Keys(vec![(Key::Enter, String::new())]));
        let shift = Modifiers { shift: true, ..NONE };
        assert_eq!(route(&input(49, " ", shift)), Route::Keys(vec![(Key::ShiftSpace, String::new())]));
        assert_eq!(route(&input(49, " ", NONE)), Route::Keys(vec![(Key::Space, String::new())]));
    }

    #[test]
    fn ordinary_letter_is_keys_with_its_text() {
        assert_eq!(
            route(&input(0, "a", NONE)),
            Route::Keys(vec![(Key::Letter { upper: false }, "a".to_string())])
        );
    }

    #[test]
    fn multiple_characters_become_one_key_each() {
        assert_eq!(
            route(&input(0, "ab", NONE)),
            Route::Keys(vec![
                (Key::Letter { upper: false }, "a".to_string()),
                (Key::Letter { upper: false }, "b".to_string()),
            ])
        );
    }

    #[test]
    fn debug_output_does_not_contain_typed_text() {
        let dbg = format!("{:?}", route(&input(0, "xyz", NONE)));
        assert!(!dbg.contains('x') && !dbg.contains('z'), "{dbg}");
    }

    #[test]
    fn シフトはスペースだけを区別し_エンターとタブは変えない() {
        assert_eq!(stroke(49, " ", true), Route::Keys(vec![(Key::ShiftSpace, String::new())]));
        assert_eq!(stroke(36, "\r", true), Route::Keys(vec![(Key::Enter, String::new())]));
        assert_eq!(stroke(48, "\u{19}", true), Route::Keys(vec![(Key::Control, String::new())]));
    }

    #[test]
    fn 特殊キーはkeycodeで_それ以外は文字で解釈する() {
        assert_eq!(stroke(49, " ", false), Route::Keys(vec![(Key::Space, String::new())]));
        assert_eq!(stroke(36, "\r", false), Route::Keys(vec![(Key::Enter, String::new())]));
        assert_eq!(stroke(76, "\u{3}", false), Route::Keys(vec![(Key::Enter, String::new())]));
        assert_eq!(stroke(125, "\u{F701}", false), Route::Keys(vec![(Key::Down, String::new())]));
        assert_eq!(stroke(126, "\u{F700}", false), Route::Keys(vec![(Key::Up, String::new())]));
        assert_eq!(stroke(51, "\u{7f}", false), Route::Keys(vec![(Key::Backspace, String::new())]));
        assert_eq!(stroke(48, "\t", false), Route::Keys(vec![(Key::Control, String::new())]));
        assert_eq!(stroke(53, "\u{1b}", false), Route::Keys(vec![(Key::Escape, String::new())]));
        assert_eq!(stroke(0, "A", false), Route::Keys(vec![(Key::Letter { upper: true }, "A".to_string())]));
        assert_eq!(stroke(18, "1", false), Route::Keys(vec![(Key::Digit(1), "1".to_string())]));
    }

    #[test]
    fn 左右キーと機能キーと文字なしの打鍵は移動扱い() {
        assert_eq!(stroke(123, "\u{F702}", false), Route::Navigation);
        assert_eq!(stroke(124, "\u{F703}", false), Route::Navigation);
        assert_eq!(stroke(117, "\u{F728}", false), Route::Navigation);
        assert_eq!(stroke(104, "", false), Route::Navigation);
    }
}
