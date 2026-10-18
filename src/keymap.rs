#![forbid(unsafe_code)]

// keymap.rs - (局面, キー) → 動作 の対応表。IMK への副作用は持たず、imk.rs が動作を実行する。

use crate::composer::Phase;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Space,
    Digit(u8),
    Letter { upper: bool },
    /// 日本語入力では句読点などに置き換わる記号 (. , ! ? [ ])。
    Punct,
    Hyphen,
    Symbol,
    Enter,
    Escape,
    Backspace,
    /// Tab など、上記以外の制御文字。
    Control,
    Up,
    Down,
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
}

pub fn action_for(phase: Phase, key: Key) -> Action {
    use Action::*;
    use Key::*;
    use Phase as P;
    match (phase, key) {
        (P::Empty, Space) => InsertSpace,
        (P::Empty, Letter { upper: true }) => StartLiteral,
        (P::Empty, Letter { upper: false }) => TypeRomaji,
        (P::Empty, Digit(_) | Punct | Hyphen | Symbol) => InsertDirect,
        (P::Empty, Enter | Escape | Backspace | Control | Up | Down) => PassThrough,

        (P::Literal, Space) => CommitThenSpace,
        (P::Literal, Letter { .. } | Digit(_) | Hyphen | Symbol) => TypeLiteral,
        (P::Literal, Punct) => CommitThenInsert,
        (P::Literal, Enter) => Commit,
        (P::Literal, Control) => CommitThenPassThrough,
        (P::Literal, Up | Down) => PassThrough,
        (P::Literal, Escape) => Revert,
        (P::Literal, Backspace) => DeleteBack,

        (P::Converting { .. }, Space | Down) => NextCandidate,
        (P::Converting { .. }, Up) => PrevCandidate,
        (P::Composing | P::Reverted, Up | Down) => PassThrough,
        (P::Composing | P::Reverted | P::Converting { .. }, Control) => CommitThenPassThrough,
        (P::Composing | P::Reverted, Space) => Convert,
        (P::Converting { window: true, .. }, Digit(n @ 1..=9)) => PickCandidate(n),
        (P::Composing | P::Reverted | P::Converting { .. }, Letter { .. }) => TypeRomaji,
        (
            P::Composing | P::Reverted | P::Converting { .. },
            Digit(_) | Punct | Hyphen | Symbol,
        ) => TypeText,
        (P::Composing | P::Reverted | P::Converting { .. }, Enter) => Commit,
        (P::Reverted, Escape) => PassThrough,
        (P::Composing | P::Converting { .. }, Escape) => Revert,
        (P::Composing | P::Reverted | P::Converting { .. }, Backspace) => DeleteBack,
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
}
