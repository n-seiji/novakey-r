#![forbid(unsafe_code)]

// keymap.rs - (局面, キー) → 動作 の対応表。IMK への副作用は持たず、imk.rs が動作を実行する。

use crate::composer::Phase;
use crate::translit::{Case, Form};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Space,
    /// Shift を押したスペース。変換中は前候補、それ以外は Space と同じ。
    ShiftSpace,
    Digit(u8),
    Letter { upper: bool },
    /// 日本語入力では句読点などに置き換わる記号 (. , ! ? [ ])。
    Punct,
    /// 和文の記号に置き換わり、スペースで異体を選べる記号 (( ) ~ / { } : ;)。
    Mark,
    Hyphen,
    Symbol,
    Enter,
    Escape,
    Backspace,
    /// Tab など、上記以外の制御文字。
    Control,
    Up,
    Down,
    /// Ctrl+J/K/;/L/: や F6〜F10 による字種変換。
    Translit(Form),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    PassThrough,
    /// 未確定文が無いときの半角スペース。
    InsertSpace,
    /// 未確定文が無いときの記号・数字をそのまま(日本語句読点/遊び変換つきで)挿入する。
    InsertDirect,
    TypeRomaji,
    /// 大文字で始まる英単語の開始。
    StartLiteral,
    TypeLiteral,
    /// 数字・記号・句読点をかな入力の続きとして追加する。
    TypeText,
    Convert,
    NextCandidate,
    PrevCandidate,
    PickCandidate(u8),
    Commit,
    /// 確定してからスペースを入れる。
    CommitThenSpace,
    /// 確定してから入力された記号を ASCII のまま入れる。
    CommitThenInsert,
    /// 確定してから、キーをアプリへ素通しする (Tab など)。
    CommitThenPassThrough,
    Revert,
    DeleteBack,
    /// 組成全体を指定の字種で表示する。
    Transliterate(Form),
}

pub fn action_for(phase: Phase, key: Key) -> Action {
    use Action::*;
    use Key::*;
    use Phase as P;
    match (phase, key) {
        (P::Empty, Translit(_)) => PassThrough,
        (_, Translit(form)) => Transliterate(form),

        (P::Empty, Space | ShiftSpace) => InsertSpace,
        (P::Empty, Letter { upper: true }) => StartLiteral,
        (P::Empty, Letter { upper: false }) => TypeRomaji,
        (P::Empty, Punct | Mark | Hyphen) => TypeText,
        (P::Empty, Digit(_) | Symbol) => InsertDirect,
        (P::Empty, Enter | Escape | Backspace | Control | Up | Down) => PassThrough,

        (P::Literal, Space | ShiftSpace) => CommitThenSpace,
        (P::Literal, Letter { .. } | Digit(_) | Hyphen | Mark | Symbol) => TypeLiteral,
        (P::Literal, Punct) => CommitThenInsert,
        (P::Literal, Enter) => Commit,
        (P::Literal, Control) => CommitThenPassThrough,
        (P::Literal, Up | Down) => PassThrough,
        (P::Literal, Escape) => Revert,
        (P::Literal, Backspace) => DeleteBack,

        (P::Dangling, Space | ShiftSpace) => Convert,
        (P::Dangling, Enter) => Commit,
        (P::Dangling, Control) => CommitThenPassThrough,
        (P::Dangling, Up | Down) => PassThrough,
        (P::Dangling, Letter { .. }) => TypeRomaji,
        (P::Dangling, Digit(_) | Punct | Mark | Hyphen | Symbol) => CommitThenInsert,
        (P::Dangling, Escape) => Revert,
        (P::Dangling, Backspace) => DeleteBack,

        (P::Converting { .. }, Space | Down) => NextCandidate,
        (P::Converting { .. }, Up | ShiftSpace) => PrevCandidate,
        (P::Composing | P::Reverted | P::Transliterated(_), Up | Down) => PassThrough,
        (P::Composing | P::Reverted | P::Converting { .. } | P::Transliterated(_), Control) => CommitThenPassThrough,
        (P::Composing | P::Reverted | P::Transliterated(_), Space | ShiftSpace) => Convert,
        (P::Converting { window: true, .. }, Digit(n @ 1..=9)) => PickCandidate(n),
        (P::Composing | P::Reverted | P::Converting { .. } | P::Transliterated(_), Letter { .. }) => TypeRomaji,
        (
            P::Composing | P::Reverted | P::Converting { .. } | P::Transliterated(_),
            Digit(_) | Punct | Mark | Hyphen | Symbol,
        ) => TypeText,
        (P::Composing | P::Reverted | P::Converting { .. } | P::Transliterated(_), Enter) => Commit,
        (P::Reverted, Escape) => PassThrough,
        (P::Composing | P::Converting { .. } | P::Transliterated(_), Escape) => Revert,
        (P::Composing | P::Reverted | P::Converting { .. } | P::Transliterated(_), Backspace) => DeleteBack,
    }
}

/// 物理キー 1 打鍵の解釈。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stroke {
    /// 動作表で引くキーと、その入力文字 (特殊キーは空)。
    Keys(Vec<(Key, String)>),
    /// ←→ や機能キー。未確定文があれば握りつぶし、無ければアプリへ素通しする。
    Navigation,
}

/// keyCode と characters から打鍵を解釈する。特殊キーは keyCode で、それ以外は文字で分類する。
pub fn stroke(key_code: u16, characters: &str, shift: bool) -> Stroke {
    let special = match key_code {
        36 | 76 => Some(Key::Enter),
        53 => Some(Key::Escape),
        51 => Some(Key::Backspace),
        48 => Some(Key::Control),
        49 if shift => Some(Key::ShiftSpace),
        49 => Some(Key::Space),
        125 => Some(Key::Down),
        126 => Some(Key::Up),
        123 | 124 => return Stroke::Navigation,
        _ => None,
    };
    if let Some(key) = special {
        return Stroke::Keys(vec![(key, String::new())]);
    }
    let is_function_key = |c: char| ('\u{F700}'..='\u{F8FF}').contains(&c);
    if characters.is_empty() || characters.chars().any(is_function_key) {
        return Stroke::Navigation;
    }
    Stroke::Keys(
        characters
            .chars()
            .map(|ch| (classify(ch), ch.to_string()))
            .collect(),
    )
}

impl Key {
    /// トレース用の名前。入力文字 (Digit の数字など) を含めない。
    pub fn name(self) -> &'static str {
        match self {
            Key::Space => "Space",
            Key::ShiftSpace => "ShiftSpace",
            Key::Digit(_) => "Digit",
            Key::Letter { upper: true } => "Letter(upper)",
            Key::Letter { upper: false } => "Letter",
            Key::Punct => "Punct",
            Key::Mark => "Mark",
            Key::Hyphen => "Hyphen",
            Key::Symbol => "Symbol",
            Key::Enter => "Enter",
            Key::Escape => "Escape",
            Key::Backspace => "Backspace",
            Key::Control => "Control",
            Key::Up => "Up",
            Key::Down => "Down",
            Key::Translit(form) => form.name(),
        }
    }
}

impl Action {
    /// トレース用の名前。候補番号など打鍵に由来する値を含めない。
    pub fn name(self) -> &'static str {
        match self {
            Action::PassThrough => "PassThrough",
            Action::InsertSpace => "InsertSpace",
            Action::InsertDirect => "InsertDirect",
            Action::TypeRomaji => "TypeRomaji",
            Action::StartLiteral => "StartLiteral",
            Action::TypeLiteral => "TypeLiteral",
            Action::TypeText => "TypeText",
            Action::Convert => "Convert",
            Action::NextCandidate => "NextCandidate",
            Action::PrevCandidate => "PrevCandidate",
            Action::PickCandidate(_) => "PickCandidate",
            Action::Commit => "Commit",
            Action::CommitThenSpace => "CommitThenSpace",
            Action::CommitThenInsert => "CommitThenInsert",
            Action::CommitThenPassThrough => "CommitThenPassThrough",
            Action::Revert => "Revert",
            Action::DeleteBack => "DeleteBack",
            Action::Transliterate(_) => "Transliterate",
        }
    }
}

/// 字種変換の機能キー (F6〜F10) の keyCode と変換先。
const TRANSLIT_FUNCTION_KEYS: &[(u16, Form)] = &[
    (97, Form::Hiragana),
    (98, Form::Katakana),
    (100, Form::HalfKatakana),
    (101, Form::FullAlnum(Case::Lower)),
    (109, Form::HalfAlnum(Case::Lower)),
];

/// Ctrl を押したときの (キー, 変換先)。キーは charactersIgnoringModifiers の小文字。
const TRANSLIT_CONTROL_KEYS: &[(&str, Form)] = &[
    ("j", Form::Hiragana),
    ("k", Form::Katakana),
    (";", Form::HalfKatakana),
    ("l", Form::FullAlnum(Case::Lower)),
    (":", Form::HalfAlnum(Case::Lower)),
    ("'", Form::HalfAlnum(Case::Lower)),
];

/// 字種変換のキーなら変換先を返す。Ctrl 押下中は文字で、それ以外は F6〜F10 を keyCode で引く。
pub fn translit_form(key_code: u16, control: bool, characters_ignoring_modifiers: &str) -> Option<Form> {
    if control {
        let key = characters_ignoring_modifiers.to_lowercase();
        TRANSLIT_CONTROL_KEYS
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, form)| *form)
    } else {
        TRANSLIT_FUNCTION_KEYS
            .iter()
            .find(|(code, _)| *code == key_code)
            .map(|(_, form)| *form)
    }
}

/// 入力された 1 文字をキーに分類する。
pub fn classify(ch: char) -> Key {
    match ch {
        '\r' | '\n' => Key::Enter,
        '\u{1b}' => Key::Escape,
        '\u{8}' | '\u{7f}' => Key::Backspace,
        c if c.is_control() => Key::Control,
        ' ' => Key::Space,
        '-' => Key::Hyphen,
        '.' | ',' | '!' | '?' | '[' | ']' => Key::Punct,
        '(' | ')' | '~' | '/' | '{' | '}' | ':' | ';' => Key::Mark,
        c if c.is_ascii_alphabetic() => Key::Letter {
            upper: c.is_ascii_uppercase(),
        },
        c => c
            .to_digit(10)
            .filter(|_| c.is_ascii_digit())
            .map_or(Key::Symbol, |d| Key::Digit(d as u8)),
    }
}

#[cfg(test)]
mod tests {
    use super::Action::*;
    use super::Key::*;
    use super::*;

    const CONVERTING: Phase = Phase::Converting {
        selected: 0,
        window: false,
    };
    const WINDOW: Phase = Phase::Converting {
        selected: 1,
        window: true,
    };

    #[test]
    fn 未確定文が無いスペースは常に半角スペース() {
        assert_eq!(action_for(Phase::Empty, Space), InsertSpace);
    }

    #[test]
    fn シフトスペースは変換中のみ前候補_それ以外はスペースと同じ() {
        assert_eq!(action_for(Phase::Empty, ShiftSpace), InsertSpace);
        assert_eq!(action_for(Phase::Literal, ShiftSpace), CommitThenSpace);
        assert_eq!(action_for(Phase::Dangling, ShiftSpace), Convert);
        assert_eq!(action_for(Phase::Composing, ShiftSpace), Convert);
        assert_eq!(action_for(Phase::Reverted, ShiftSpace), Convert);
        assert_eq!(action_for(CONVERTING, ShiftSpace), PrevCandidate);
        assert_eq!(action_for(WINDOW, ShiftSpace), PrevCandidate);
    }

    #[test]
    fn シフトはスペースだけを区別し_エンターとタブは変えない() {
        assert_eq!(stroke(49, " ", true), Stroke::Keys(vec![(ShiftSpace, String::new())]));
        assert_eq!(stroke(36, "\r", true), Stroke::Keys(vec![(Enter, String::new())]));
        assert_eq!(stroke(48, "\u{19}", true), Stroke::Keys(vec![(Control, String::new())]));
    }

    #[test]
    fn 未変換のスペースは変換_変換中のスペースは次候補() {
        assert_eq!(action_for(Phase::Composing, Space), Convert);
        assert_eq!(action_for(Phase::Reverted, Space), Convert);
        assert_eq!(action_for(CONVERTING, Space), NextCandidate);
        assert_eq!(action_for(WINDOW, Space), NextCandidate);
    }

    #[test]
    fn 数字キーは候補ウィンドウ表示中のみ候補選択() {
        assert_eq!(action_for(WINDOW, Digit(3)), PickCandidate(3));
        assert_eq!(action_for(WINDOW, Digit(0)), TypeText);
        assert_eq!(action_for(CONVERTING, Digit(3)), TypeText);
        assert_eq!(action_for(Phase::Composing, Digit(3)), TypeText);
        assert_eq!(action_for(Phase::Empty, Digit(3)), InsertDirect);
    }

    #[test]
    fn 未確定文が無いとき記号は組成を始め_数字と他の記号は直接入力() {
        for key in [Punct, Mark, Hyphen] {
            assert_eq!(action_for(Phase::Empty, key), TypeText);
        }
        assert_eq!(action_for(Phase::Empty, Digit(0)), InsertDirect);
        assert_eq!(action_for(Phase::Empty, Symbol), InsertDirect);
    }

    #[test]
    fn 記号の続きは組成に加え_英単語入力中はasciiのまま() {
        for phase in [Phase::Composing, Phase::Reverted, CONVERTING, WINDOW] {
            assert_eq!(action_for(phase, Mark), TypeText);
            assert_eq!(action_for(phase, Punct), TypeText);
        }
        assert_eq!(action_for(Phase::Literal, Mark), TypeLiteral);
        assert_eq!(action_for(Phase::Literal, Punct), CommitThenInsert);
        assert_eq!(action_for(Phase::Dangling, Mark), CommitThenInsert);
    }

    #[test]
    fn 先頭の大文字は英単語入力を開始し_小文字はローマ字() {
        assert_eq!(action_for(Phase::Empty, Letter { upper: true }), StartLiteral);
        assert_eq!(action_for(Phase::Empty, Letter { upper: false }), TypeRomaji);
        assert_eq!(action_for(Phase::Composing, Letter { upper: true }), TypeRomaji);
    }

    #[test]
    fn 英単語入力中は文字と数字とハイフンを続け_スペースで確定して区切る() {
        assert_eq!(action_for(Phase::Literal, Letter { upper: false }), TypeLiteral);
        assert_eq!(action_for(Phase::Literal, Digit(1)), TypeLiteral);
        assert_eq!(action_for(Phase::Literal, Hyphen), TypeLiteral);
        assert_eq!(action_for(Phase::Literal, Space), CommitThenSpace);
        assert_eq!(action_for(Phase::Literal, Punct), CommitThenInsert);
        assert_eq!(action_for(Phase::Literal, Enter), Commit);
    }

    #[test]
    fn enterは表示中の内容を確定し_空なら素通し() {
        for phase in [Phase::Composing, Phase::Reverted, CONVERTING, WINDOW] {
            assert_eq!(action_for(phase, Enter), Commit);
        }
        assert_eq!(action_for(Phase::Empty, Enter), PassThrough);
    }

    #[test]
    fn tabなどの制御文字は未確定文があれば確定して素通し_無ければ素通し() {
        for phase in [Phase::Composing, Phase::Reverted, Phase::Literal, CONVERTING, WINDOW] {
            assert_eq!(action_for(phase, Control), CommitThenPassThrough);
        }
        assert_eq!(action_for(Phase::Empty, Control), PassThrough);
    }

    #[test]
    fn 上下キーは変換中のみ候補移動_それ以外は素通し() {
        assert_eq!(action_for(CONVERTING, Down), NextCandidate);
        assert_eq!(action_for(WINDOW, Down), NextCandidate);
        assert_eq!(action_for(CONVERTING, Up), PrevCandidate);
        assert_eq!(action_for(WINDOW, Up), PrevCandidate);
        for phase in [Phase::Empty, Phase::Composing, Phase::Reverted, Phase::Literal] {
            assert_eq!(action_for(phase, Up), PassThrough);
            assert_eq!(action_for(phase, Down), PassThrough);
        }
    }

    #[test]
    fn escは一度だけ変換を戻し_戻した後は素通し() {
        assert_eq!(action_for(Phase::Composing, Escape), Revert);
        assert_eq!(action_for(WINDOW, Escape), Revert);
        assert_eq!(action_for(Phase::Reverted, Escape), PassThrough);
        assert_eq!(action_for(Phase::Empty, Escape), PassThrough);
    }

    #[test]
    fn backspaceは未確定文があれば1文字削除_無ければ素通し() {
        assert_eq!(action_for(Phase::Empty, Backspace), PassThrough);
        assert_eq!(action_for(WINDOW, Backspace), DeleteBack);
        assert_eq!(action_for(Phase::Literal, Backspace), DeleteBack);
    }

    #[test]
    fn 入力文字をキーに分類する() {
        assert_eq!(classify(' '), Space);
        assert_eq!(classify('7'), Digit(7));
        assert_eq!(classify('a'), Letter { upper: false });
        assert_eq!(classify('A'), Letter { upper: true });
        assert_eq!(classify('.'), Punct);
        assert_eq!(classify('-'), Hyphen);
        for ch in "()~/{}:;".chars() {
            assert_eq!(classify(ch), Mark, "{ch}");
        }
        assert_eq!(classify('@'), Symbol);
        assert_eq!(classify('７'), Symbol);
    }

    #[test]
    fn 制御文字はテキストではなくキーに分類する() {
        assert_eq!(classify('\r'), Enter);
        assert_eq!(classify('\n'), Enter);
        assert_eq!(classify('\u{1b}'), Escape);
        assert_eq!(classify('\u{8}'), Backspace);
        assert_eq!(classify('\u{7f}'), Backspace);
        assert_eq!(classify('\t'), Control);
        assert_eq!(classify('\u{3}'), Control);
    }

    #[test]
    fn 特殊キーはkeycodeで_それ以外は文字で解釈する() {
        assert_eq!(stroke(49, " ", false), Stroke::Keys(vec![(Space, String::new())]));
        assert_eq!(stroke(36, "\r", false), Stroke::Keys(vec![(Enter, String::new())]));
        assert_eq!(stroke(76, "\u{3}", false), Stroke::Keys(vec![(Enter, String::new())]));
        assert_eq!(stroke(125, "\u{F701}", false), Stroke::Keys(vec![(Down, String::new())]));
        assert_eq!(stroke(126, "\u{F700}", false), Stroke::Keys(vec![(Up, String::new())]));
        assert_eq!(stroke(51, "\u{7f}", false), Stroke::Keys(vec![(Backspace, String::new())]));
        assert_eq!(stroke(48, "\t", false), Stroke::Keys(vec![(Control, String::new())]));
        assert_eq!(stroke(53, "\u{1b}", false), Stroke::Keys(vec![(Escape, String::new())]));
        assert_eq!(stroke(0, "A", false), Stroke::Keys(vec![(Letter { upper: true }, "A".to_string())]));
        assert_eq!(stroke(18, "1", false), Stroke::Keys(vec![(Digit(1), "1".to_string())]));
    }

    #[test]
    fn 左右キーと機能キーと文字なしの打鍵は移動扱い() {
        assert_eq!(stroke(123, "\u{F702}", false), Stroke::Navigation);
        assert_eq!(stroke(124, "\u{F703}", false), Stroke::Navigation);
        assert_eq!(stroke(117, "\u{F728}", false), Stroke::Navigation);
        assert_eq!(stroke(104, "", false), Stroke::Navigation);
    }

    #[test]
    fn トレース用の名前は入力文字を含まない() {
        assert_eq!(Digit(7).name(), "Digit");
        assert_eq!(PickCandidate(7).name(), "PickCandidate");
    }

    #[test]
    fn 字種変換は未確定文があれば実行し_無ければ素通し() {
        let form = Form::Katakana;
        assert_eq!(action_for(Phase::Empty, Translit(form)), PassThrough);
        for phase in [
            Phase::Composing,
            Phase::Reverted,
            Phase::Literal,
            Phase::Dangling,
            CONVERTING,
            WINDOW,
            Phase::Transliterated(Form::Hiragana),
        ] {
            assert_eq!(action_for(phase, Translit(form)), Transliterate(form));
        }
    }

    #[test]
    fn 字種変換中はconverting相当の操作になる() {
        let phase = Phase::Transliterated(Form::Katakana);
        assert_eq!(action_for(phase, Enter), Commit);
        assert_eq!(action_for(phase, Escape), Revert);
        assert_eq!(action_for(phase, Backspace), DeleteBack);
        assert_eq!(action_for(phase, Space), Convert);
        assert_eq!(action_for(phase, ShiftSpace), Convert);
        assert_eq!(action_for(phase, Letter { upper: false }), TypeRomaji);
        assert_eq!(action_for(phase, Digit(1)), TypeText);
        assert_eq!(action_for(phase, Mark), TypeText);
        assert_eq!(action_for(phase, Control), CommitThenPassThrough);
        assert_eq!(action_for(phase, Up), PassThrough);
    }

    #[test]
    fn ctrlキーと機能キーを変換先に対応させる() {
        let lower = Case::Lower;
        for (chars, want) in [
            ("j", Form::Hiragana),
            ("k", Form::Katakana),
            ("K", Form::Katakana),
            (";", Form::HalfKatakana),
            ("l", Form::FullAlnum(lower)),
            (":", Form::HalfAlnum(lower)),
            ("'", Form::HalfAlnum(lower)),
        ] {
            assert_eq!(translit_form(40, true, chars), Some(want), "ctrl+{chars}");
        }
        for (code, want) in [
            (97, Form::Hiragana),
            (98, Form::Katakana),
            (100, Form::HalfKatakana),
            (101, Form::FullAlnum(lower)),
            (109, Form::HalfAlnum(lower)),
        ] {
            assert_eq!(translit_form(code, false, ""), Some(want), "keyCode {code}");
        }
    }

    #[test]
    fn 字種変換に当たらないキーはnone() {
        assert_eq!(translit_form(0, true, "a"), None);
        assert_eq!(translit_form(40, false, "k"), None);
        assert_eq!(translit_form(97, true, ""), None);
        assert_eq!(translit_form(96, false, ""), None);
    }
}
