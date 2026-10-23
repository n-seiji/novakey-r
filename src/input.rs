#![forbid(unsafe_code)]
//! Cocoa を知らないキー入力の表現と、その振り分け。imk.rs (実機) と simulate.rs (headless) が共有する。
use crate::keymap::{self, Key, Stroke};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Command / Option、字種変換キーでない Ctrl。IME は触らずアプリへ渡す。
    PassThrough,
    Translit(Form),
    /// どれか 1 つでも処理したら全体を処理済みとして扱う (未処理キーの文字は捨てる)。
    Keys(Vec<(Key, String)>),
    /// ←→ や機能キー。未確定文があれば握りつぶし、無ければアプリへ素通しする (判定は呼び出し側)。
    Navigation,
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
    match keymap::stroke(input.key_code, &input.characters, input.mods.shift) {
        Stroke::Keys(keys) => Route::Keys(keys),
        Stroke::Navigation => Route::Navigation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let lower = route(&input(0, "k", m));
        assert!(matches!(lower, Route::Translit(_)), "{lower:?}");
        assert_eq!(route(&input(0, "K", m)), lower);
    }

    #[test]
    fn control_other_keys_pass_through() {
        let m = Modifiers { control: true, ..NONE };
        assert_eq!(route(&input(0, "a", m)), Route::PassThrough);
        assert_eq!(route(&input(36, "\r", m)), Route::PassThrough);
    }

    #[test]
    fn function_keys_f6_to_f10_become_translit() {
        for (code, ch) in [(97, "\u{F709}"), (98, "\u{F70A}"), (100, "\u{F70B}"), (101, "\u{F70C}"), (109, "\u{F70D}")] {
            let r = route(&input(code, ch, NONE));
            assert!(matches!(r, Route::Translit(_)), "key_code {code}: {r:?}");
        }
    }

    #[test]
    fn left_and_right_arrows_are_navigation() {
        assert_eq!(route(&input(123, "\u{F702}", NONE)), Route::Navigation);
        assert_eq!(route(&input(124, "\u{F703}", NONE)), Route::Navigation);
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
        match route(&input(0, "a", NONE)) {
            Route::Keys(keys) => {
                assert_eq!(keys.len(), 1);
                assert_eq!(keys[0].1, "a");
            }
            other => panic!("{other:?}"),
        }
    }
}
