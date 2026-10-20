#![forbid(unsafe_code)]

// session.rs - 1 キーぶんの判断。ローマ字バッファと composer を持ち、キーを受けて状態を進め、
// Cocoa に対して行うべきこと (Effect) の列を返す。imk.rs は Effect を実行するだけで、ここは headless で動く。

use std::collections::HashMap;

use once_cell::sync::Lazy;
use rand::Rng;

use crate::composer::{CandidateEngine, Composer, Phase};
use crate::keymap::{self, Action, Key};
use crate::romaji_converter::{Flush, RomajiConverter};
use crate::symbols;

/// リランキング要求の中身。送信は Effect の実行側が決める。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RerankJob {
    pub generation: u64,
    pub hiragana: String,
    pub candidates: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    SetMarked(String),
    Insert(String),
    /// 候補ウィンドウを出し、選択中の行 (0 起点) をハイライトする。
    ShowCandidates { selected: usize },
    HideCandidates,
    /// urgent は Space 押下時用。デバウンスを待たずに送る。
    RequestRerank { urgent: bool, job: RerankJob },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// アプリへ素通しせず処理したか。
    pub handled: bool,
    /// 動作表で引いた動作。ローマ字バッファ側で済んだときは None。
    pub action: Option<Action>,
    /// 動作を決めた時点の局面 (ローマ字バッファの確定後)。
    pub phase: Phase,
    pub effects: Vec<Effect>,
}

pub struct Session<E: CandidateEngine> {
    pub converter: RomajiConverter,
    pub composer: Composer<E>,
    /// true のときだけ RequestRerank を出す。
    pub rerank: bool,
    /// 未確定文が無いときの記号・数字の「遊び」変換。テストでは決定的なものに差し替える。
    pub playful: fn(&str) -> Option<String>,
}

impl<E: CandidateEngine> Session<E> {
    pub fn new(composer: Composer<E>) -> Self {
        Self {
            converter: RomajiConverter::new(),
            composer,
            rerank: false,
            playful: playful_convert,
        }
    }

    pub fn phase(&self) -> Phase {
        self.composer.phase()
    }

    /// 表示中の変換結果(またはかな) + 未変換のローマ字。
    pub fn marked_text(&self) -> String {
        format!(
            "{}{}",
            self.composer.display(),
            self.converter.pending_display()
        )
    }

    /// キー処理の入口。キーを動作表で解決して実行する。
    pub fn handle(&mut self, key: Key, text: &str) -> Outcome {
        let mut fx = Vec::new();
        let phase = self.phase();
        // `n'` のように、ローマ字表が記号を含む打鍵列は記号キーでも表を優先する。
        let key = match text.chars().collect::<Vec<_>>()[..] {
            [ch] if !ch.is_ascii_alphabetic() && self.converter.accepts(ch) => {
                Key::Letter { upper: false }
            }
            _ => key,
        };
        if let Some(handled) = self.prepare(key, &mut fx) {
            return Outcome { handled, action: None, phase, effects: fx };
        }
        let phase = self.phase();
        let action = keymap::action_for(phase, key);
        let handled = self.perform(action, text, &mut fx);
        Outcome { handled, action: Some(action), phase, effects: fx }
    }

    /// 候補ウィンドウでの選択を確定する (candidateSelected:)。
    pub fn commit_selected(&mut self, text: &str) -> Vec<Effect> {
        self.composer.select_candidate_text(text);
        let mut fx = Vec::new();
        self.commit_and_insert("", &mut fx);
        fx
    }

    /// 入力ソースの切り替えなどによる確定 (commitComposition:)。
    pub fn commit_composition(&mut self) -> Vec<Effect> {
        let mut fx = Vec::new();
        self.flush_romaji(true, &mut fx);
        if !self.composer.is_empty() {
            fx.push(Effect::Insert(self.composer.commit()));
        }
        fx.push(Effect::HideCandidates);
        fx
    }

    fn rerank_job(&self) -> Option<RerankJob> {
        let composer = &self.composer;
        (composer.phase() == Phase::Composing
            && !composer.is_symbol()
            && composer.candidates().len() >= 2)
            .then(|| RerankJob {
                generation: composer.current_generation(),
                hiragana: composer.display_hiragana().to_string(),
                candidates: composer.candidates().iter().map(|c| c.text.clone()).collect(),
            })
    }

    fn request_rerank(&self, urgent: bool, fx: &mut Vec<Effect>) {
        if !self.rerank {
            return;
        }
        if let Some(job) = self.rerank_job() {
            fx.push(Effect::RequestRerank { urgent, job });
        }
    }

    fn refresh_marked(&self, fx: &mut Vec<Effect>) {
        fx.push(Effect::SetMarked(self.marked_text()));
    }

    /// 未変換のローマ字を確定かなとして composer へ流す。
    fn flush_romaji(&mut self, rerank: bool, fx: &mut Vec<Effect>) {
        let flush = self.converter.flush();
        let ranks = matches!(flush, Flush::Kana(_));
        self.composer.push_flush(flush);
        if ranks && rerank {
            self.request_rerank(false, fx);
        }
    }

    fn push_input(&mut self, raw: &str, kana: &str, fx: &mut Vec<Effect>) {
        let carry = self.converter.buffer().to_string();
        self.composer.push_input(raw, kana, &carry);
        if !kana.is_empty() {
            self.request_rerank(false, fx);
        }
    }

    /// 現在の表示内容を確定して挿入する。suffix は確定直後に続けて入れる文字列。
    fn commit_and_insert(&mut self, suffix: &str, fx: &mut Vec<Effect>) {
        let text = self.composer.commit();
        fx.push(Effect::SetMarked(String::new()));
        fx.push(Effect::Insert(format!("{text}{suffix}")));
        fx.push(Effect::HideCandidates);
    }

    /// 入力途中の表示を更新して候補ウィンドウを閉じる。
    fn show_typing(&self, fx: &mut Vec<Effect>) -> bool {
        self.refresh_marked(fx);
        fx.push(Effect::HideCandidates);
        true
    }

    /// 選択中の候補を表示し、候補ウィンドウを出してハイライトを合わせる。
    fn show_selection(&self, fx: &mut Vec<Effect>) -> bool {
        self.refresh_marked(fx);
        if let Phase::Converting { selected, .. } = self.phase() {
            fx.push(Effect::ShowCandidates { selected });
        }
        true
    }

    /// 動作表に引く前の、ローマ字バッファ側の後始末。ここで処理が済んだら Some。
    fn prepare(&mut self, key: Key, fx: &mut Vec<Effect>) -> Option<bool> {
        let flushes = matches!(
            key,
            Key::Space
                | Key::ShiftSpace
                | Key::Digit(_)
                | Key::Punct
                | Key::Mark
                | Key::Hyphen
                | Key::Symbol
                | Key::Enter
                | Key::Control
                | Key::Letter { upper: true }
                | Key::Translit(_)
        );
        if flushes {
            self.flush_romaji(!matches!(key, Key::Translit(_)), fx);
        }
        let has_romaji = !self.converter.buffer().is_empty();
        match key {
            Key::Backspace if has_romaji => {
                self.converter.handle_backspace();
                self.composer.pop_pending_raw();
                self.refresh_marked(fx);
                Some(true)
            }
            Key::Escape => {
                self.converter.clear();
                self.composer.clear_pending_raw();
                if has_romaji && self.composer.is_empty() {
                    fx.push(Effect::SetMarked(String::new()));
                    return Some(true);
                }
                None
            }
            _ => None,
        }
    }

    fn perform(&mut self, action: Action, text: &str, fx: &mut Vec<Effect>) -> bool {
        match action {
            Action::PassThrough => false,
            Action::InsertSpace => {
                fx.push(Effect::Insert(" ".to_string()));
                true
            }
            Action::InsertDirect => (self.playful)(text)
                .map(|converted| fx.push(Effect::Insert(converted)))
                .is_some(),
            Action::TypeRomaji => {
                let kana = text
                    .chars()
                    .filter_map(|ch| self.converter.process_input(ch.to_ascii_lowercase()))
                    .collect::<String>();
                self.push_input(text, &kana, fx);
                self.show_typing(fx)
            }
            Action::StartLiteral | Action::TypeLiteral => {
                self.composer.push_literal(text);
                self.show_typing(fx)
            }
            Action::TypeText => {
                self.push_input(text, symbols::default_form(text).unwrap_or(text), fx);
                self.show_typing(fx)
            }
            Action::Convert => {
                let needs_rerank =
                    self.composer.phase() != Phase::Dangling && !self.composer.has_pending_rerank();
                if needs_rerank {
                    self.request_rerank(true, fx);
                }
                self.composer.begin_conversion();
                self.refresh_marked(fx);
                true
            }
            Action::NextCandidate => {
                self.composer.advance();
                self.show_selection(fx)
            }
            Action::PrevCandidate => {
                self.composer.retreat();
                self.show_selection(fx)
            }
            Action::CommitThenPassThrough => {
                self.commit_and_insert("", fx);
                false
            }
            Action::PickCandidate(number) => {
                if self.composer.select_candidate(usize::from(number) - 1) {
                    self.commit_and_insert("", fx);
                }
                true
            }
            Action::Commit => {
                self.commit_and_insert("", fx);
                true
            }
            Action::CommitThenSpace => {
                self.commit_and_insert(" ", fx);
                true
            }
            Action::CommitThenInsert => {
                self.commit_and_insert(text, fx);
                true
            }
            Action::Revert => {
                self.composer.revert();
                self.show_typing(fx)
            }
            Action::Transliterate(form) => {
                self.composer.transliterate(form) && self.show_typing(fx)
            }
            Action::DeleteBack => {
                let deleted = self.composer.backspace();
                if deleted {
                    self.request_rerank(false, fx);
                    self.show_typing(fx);
                }
                deleted
            }
        }
    }
}

static PLAYFUL_MAP: Lazy<HashMap<&'static str, Vec<&'static str>>> = Lazy::new(|| {
    let mut outs = HashMap::new();
    outs.insert("l", vec!["l", "I", "|"]);
    outs.insert("1", vec!["l", "1", "I"]);
    outs.insert("I", vec!["l", "I", "|"]);
    outs.insert("O", vec!["O", "0"]);
    outs.insert("0", vec!["O", "0"]);
    outs
});

pub fn playful_convert(text: &str) -> Option<String> {
    let list = PLAYFUL_MAP.get(text)?;
    let i = rand::thread_rng().gen_range(0..list.len());
    Some(list[i].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::classify;

    struct NullEngine;
    impl CandidateEngine for NullEngine {
        fn convert(&mut self, _: u64, _: &str) -> Vec<String> {
            Vec::new()
        }
        fn select(&mut self, _: u64, _: usize) -> bool {
            false
        }
        fn cancel(&mut self) {}
    }

    /// 打鍵列を流して、最後に Enter で確定した文字列を返す。
    fn typed(keys: &str) -> String {
        let mut session = Session::new(Composer::new(NullEngine));
        let mut out = String::new();
        for ch in keys.chars().chain(['\r']) {
            let outcome = session.handle(classify(ch), &ch.to_string());
            for effect in outcome.effects {
                if let Effect::Insert(text) = effect {
                    out.push_str(&text);
                }
            }
        }
        out
    }

    #[test]
    fn アポストロフィでnを確定して次の母音を切り離せる() {
        assert_eq!(typed("kin'en"), "きんえん");
        assert_eq!(typed("n'a"), "んあ");
    }

    #[test]
    fn 表に続かないアポストロフィは記号として入る() {
        assert_eq!(typed("a'"), "あ'");
        assert_eq!(typed("t'i"), "てぃ");
    }
}
