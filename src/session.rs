#![forbid(unsafe_code)]

// session.rs - 1 キーぶんの判断。ローマ字バッファと composer を持ち、キーを受けて状態を進め、
// Cocoa に対して行うべきこと (Effect) の列を返す。imk.rs は Effect を実行するだけで、ここは headless で動く。

use std::collections::HashMap;
use std::sync::LazyLock;

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
    /// 組成が終わった。待機中・送信中の要求を捨てる。
    CancelRerank,
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
    /// RequestRerank を出してから、まだ CancelRerank を出していない。
    rerank_outstanding: bool,
}

impl<E: CandidateEngine> Session<E> {
    pub fn new(composer: Composer<E>) -> Self {
        Self {
            converter: RomajiConverter::new(),
            composer,
            rerank: false,
            playful: playful_convert,
            rerank_outstanding: false,
        }
    }

    pub fn phase(&self) -> Phase {
        self.composer.phase()
    }

    /// 表示中の変換結果(またはかな) + 未変換のローマ字。
    pub fn marked_text(&self) -> String {
        if let Some(split) = self.composer.english_split(false) {
            return split.display();
        }
        if self.composer.is_typing_english() {
            return self.composer.raw();
        }
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
            self.finish(&mut fx);
            return Outcome { handled, action: None, phase, effects: fx };
        }
        let phase = self.phase();
        let action = keymap::action_for(phase, key);
        if action == Action::CommitThenRestart {
            self.commit_and_insert("", &mut fx);
            let restarted = self.handle(key, text);
            fx.extend(restarted.effects);
            self.finish(&mut fx);
            return Outcome { handled: restarted.handled, action: Some(action), phase, effects: fx };
        }
        let handled = self.perform(action, text, &mut fx);
        self.finish(&mut fx);
        Outcome { handled, action: Some(action), phase, effects: fx }
    }

    /// 候補ウィンドウでの選択を確定する (candidateSelected:)。
    pub fn commit_selected(&mut self, text: &str) -> Vec<Effect> {
        self.composer.select_candidate_text(text);
        let mut fx = Vec::new();
        self.commit_and_insert("", &mut fx);
        self.finish(&mut fx);
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
        self.finish(&mut fx);
        fx
    }

    /// 組成が空になっていて、AI 要求を出したままなら、その取り消しを 1 度だけ出す。
    fn finish(&mut self, fx: &mut Vec<Effect>) {
        if self.rerank_outstanding && self.composer.is_empty() && self.converter.buffer().is_empty() {
            self.rerank_outstanding = false;
            fx.push(Effect::CancelRerank);
        }
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

    fn request_rerank(&mut self, urgent: bool, fx: &mut Vec<Effect>) {
        if !self.rerank {
            return;
        }
        if let Some(job) = self.rerank_job() {
            self.rerank_outstanding = true;
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
            // handle が先に処理するのでここには来ない。
            Action::CommitThenRestart => false,
            Action::Clear => {
                self.composer.discard();
                self.converter.clear();
                self.show_typing(fx)
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

static PLAYFUL_MAP: LazyLock<HashMap<&'static str, Vec<&'static str>>> = LazyLock::new(|| {
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
    Some(list[random_index(list.len())].to_string())
}

// RandomState は生成のたびに別のキーを持つので、呼び出しごとに新しく作ると毎回違う値になる
fn random_index(len: usize) -> usize {
    use std::hash::{BuildHasher, Hasher};
    let hash = std::collections::hash_map::RandomState::new().build_hasher().finish();
    (hash % len as u64) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_index_stays_within_len() {
        for len in [1usize, 2, 3, 1000] {
            for _ in 0..2000 {
                assert!(random_index(len) < len);
            }
        }
    }
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

    fn english_session() -> Session<NullEngine> {
        Session::new(
            Composer::new(NullEngine).with_english_word(|w| matches!(w, "enter" | "rain" | "in")),
        )
    }

    /// 打鍵列を流し、各打鍵後の表示と、最後に Enter で確定した文字列を返す。
    fn typed_english(keys: &str) -> (Vec<String>, String) {
        let mut session = english_session();
        let mut shown = Vec::new();
        let mut out = String::new();
        for ch in keys.chars().chain(['\r']) {
            for effect in session.handle(classify(ch), &ch.to_string()).effects {
                if let Effect::Insert(text) = effect {
                    out.push_str(&text);
                }
            }
            shown.push(session.marked_text());
        }
        shown.pop();
        (shown, out)
    }

    #[test]
    fn 英単語を打ち終えた時点で英字で表示しenterで英字を確定する() {
        let (shown, out) = typed_english("enter");
        assert_eq!(shown, ["え", "えn", "えんt", "えんて", "enter"]);
        assert_eq!(out, "enter");
    }

    #[test]
    fn nで終わる英単語もenterで英字を確定する() {
        let (shown, out) = typed_english("rain");
        assert_eq!(shown.last().unwrap(), "rain");
        assert_eq!(out, "rain");
    }

    #[test]
    fn 英単語の続きを打てばかなに戻る() {
        let (shown, out) = typed_english("rainen");
        assert_eq!(shown[3], "rain");
        assert_eq!(shown.last().unwrap(), "らいねn");
        assert_eq!(out, "らいねん");
    }

    #[test]
    fn かなの途中に変換できない子音が挟まれば辞書に無い語も英字で表示する() {
        let (shown, out) = typed_english("merge");
        assert_eq!(shown, ["m", "め", "めr", "merg", "merge"]);
        assert_eq!(out, "merge");
        let (shown, _) = typed_english("branch");
        assert_eq!(shown[2], "bra");
    }

    thread_local! {
        static LEARNED: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    fn remembering_session() -> Session<NullEngine> {
        Session::new(Composer::new(NullEngine).with_english_memory(
            |w| LEARNED.with(|l| l.borrow().iter().any(|x| x == w)),
            |w| LEARNED.with(|l| l.borrow_mut().push(w.to_string())),
        ))
    }

    fn shown_after(session: &mut Session<NullEngine>, keys: &str) -> String {
        for ch in keys.chars() {
            session.handle(classify(ch), &ch.to_string());
        }
        session.marked_text()
    }

    #[test]
    fn 英語として確定した語は次から打ちかけでも英字で表示する() {
        let mut session = remembering_session();
        assert_eq!(shown_after(&mut session, "git"), "ぎt");
        session.handle(Key::Enter, "\r");
        assert_eq!(shown_after(&mut session, "git"), "git");
        assert_eq!(shown_after(&mut session, "a"), "ぎた");
    }

    #[test]
    fn かなで確定した語は覚えない() {
        let mut session = remembering_session();
        shown_after(&mut session, "gita");
        session.handle(Key::Enter, "\r");
        assert_eq!(shown_after(&mut session, "git"), "ぎt");
    }

    #[test]
    fn 英単語のあとの助詞はかなで表示しenterでそのまま確定する() {
        let (shown, out) = typed_english("artifactni");
        assert_eq!(shown.last().unwrap(), "artifactに");
        assert_eq!(out, "artifactに");
    }

    #[test]
    fn 英単語のあとの助詞はスペースで変換に入り候補は一度ずつ() {
        let mut session = english_session();
        shown_after_english(&mut session, "artifactni ");
        assert_eq!(session.marked_text(), "artifactに");
        let texts: Vec<&str> = session.composer.candidates().iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, ["artifactに", "artifactni"]);
    }

    fn shown_after_english(session: &mut Session<NullEngine>, keys: &str) {
        for ch in keys.chars() {
            session.handle(classify(ch), &ch.to_string());
        }
    }

    #[test]
    fn 三文字以下の英単語はかなのまま() {
        let (shown, out) = typed_english("inu");
        assert_eq!(shown[1], "いn");
        assert_eq!(out, "いぬ");
    }

    #[test]
    fn 英語の途中のスペースは英字を確定して空白を入れる() {
        assert_eq!(typed_english("rain merge").1, "rain merge");
    }

    #[test]
    fn 英語の途中のシフトスペースは英字を第1候補にして変換する() {
        let mut session = english_session();
        for ch in "rain".chars() {
            session.handle(classify(ch), &ch.to_string());
        }
        session.handle(Key::ShiftSpace, " ");
        assert_eq!(session.marked_text(), "rain");
        assert!(matches!(session.phase(), Phase::Converting { .. }));
    }

    #[test]
    fn 辞書がなくても_kyouha_スペース_エンターはきょうはを確定する() {
        assert_eq!(typed("kyouha "), "きょうは");
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

    /// 打鍵列を流し、(アプリへ確定済みの文字列, 下線付きの未確定文字列) を返す。Enter は付けない。
    fn after(keys: &str) -> (String, String) {
        let mut session = Session::new(Composer::new(NullEngine));
        session.playful = |text| Some(text.to_string());
        let (mut committed, mut marked) = (String::new(), String::new());
        for ch in keys.chars() {
            let outcome = session.handle(classify(ch), &ch.to_string());
            for effect in outcome.effects {
                match effect {
                    Effect::SetMarked(text) => marked = text,
                    Effect::Insert(text) => committed.push_str(&text),
                    _ => {}
                }
            }
        }
        (committed, marked)
    }

    #[test]
    fn 変換後の打鍵は表示中の候補を確定して新しい組成を始める() {
        // NullEngine は候補を返さないので、変換の第 1 候補はひらがな (raw ではない) になる。
        assert_eq!(after("kyou a"), ("きょう".to_string(), "あ".to_string()));
        assert_eq!(after("kyou 1"), ("きょう1".to_string(), String::new()));
        assert_eq!(after("kyou ."), ("きょう".to_string(), "。".to_string()));
        assert_eq!(after("kyou K"), ("きょう".to_string(), "K".to_string()));
    }

    #[test]
    fn 変換中のbackspaceは読み全体に戻り_組成中は1文字消す() {
        assert_eq!(after("kyou \u{8}"), (String::new(), "きょう".to_string()));
        assert_eq!(after("kyou\u{8}"), (String::new(), "きょ".to_string()));
    }

    #[test]
    fn escは変換中なら読みに戻し_組成中なら捨てる() {
        assert_eq!(after("kyou \u{1b}"), (String::new(), "きょう".to_string()));
        assert_eq!(after("kyou \u{1b}\u{1b}"), (String::new(), String::new()));
        assert_eq!(after("kyou\u{1b}"), (String::new(), String::new()));
        assert_eq!(after("Tokyo\u{1b}"), (String::new(), String::new()));
        assert_eq!(after("kyouk\u{1b}"), (String::new(), String::new()));
    }

    #[test]
    fn 組成の途中の大文字と続く英字はそのまま入る() {
        assert_eq!(after("kyouTokyo"), (String::new(), "きょうTokyo".to_string()));
        assert_eq!(typed("kyouTokyo"), "きょうTokyo");
        assert_eq!(typed("Tokyo"), "Tokyo");
        assert_eq!(after("kyouT\u{8}"), (String::new(), "きょう".to_string()));
    }

    struct TwoEngine;
    impl CandidateEngine for TwoEngine {
        fn convert(&mut self, _: u64, hiragana: &str) -> Vec<String> {
            vec![hiragana.to_string(), "x".to_string()]
        }
        fn select(&mut self, _: u64, _: usize) -> bool {
            false
        }
        fn cancel(&mut self) {}
    }

    fn effects_of(session: &mut Session<TwoEngine>, keys: &str) -> Vec<Effect> {
        keys.chars()
            .flat_map(|ch| session.handle(classify(ch), &ch.to_string()).effects)
            .collect()
    }

    #[test]
    fn 確定したらai要求の取り消しを1度だけ出す() {
        let mut session = Session::new(Composer::new(TwoEngine));
        session.rerank = true;
        let typing = effects_of(&mut session, "kyou");
        assert!(typing.iter().any(|e| matches!(e, Effect::RequestRerank { .. })));
        assert!(!typing.contains(&Effect::CancelRerank), "組成中は取り消さない");
        let commit = effects_of(&mut session, "\r");
        assert_eq!(commit.iter().filter(|e| **e == Effect::CancelRerank).count(), 1);
        let again = effects_of(&mut session, "\r");
        assert!(!again.contains(&Effect::CancelRerank), "取り消しは 1 度だけ");
    }

    #[test]
    fn escで組成を捨てても取り消しを出す() {
        let mut session = Session::new(Composer::new(TwoEngine));
        session.rerank = true;
        effects_of(&mut session, "kyou");
        assert!(effects_of(&mut session, "\u{1b}").contains(&Effect::CancelRerank));
    }

    #[test]
    fn commit_compositionでも取り消しを出す() {
        let mut session = Session::new(Composer::new(TwoEngine));
        session.rerank = true;
        effects_of(&mut session, "kyou");
        assert!(session.commit_composition().contains(&Effect::CancelRerank));
    }

    #[test]
    fn aiが無効なら取り消しも出さない() {
        let mut session = Session::new(Composer::new(TwoEngine));
        effects_of(&mut session, "kyou");
        assert!(!effects_of(&mut session, "\r").contains(&Effect::CancelRerank));
    }

    /// `<space>` `<bs>` `<esc>` `<enter>` `<F7>` を含む打鍵列を流し、(最後の表示, 確定した文字列) を返す。
    fn drive(session: &mut Session<NullEngine>, keys: &str) -> (String, String) {
        let mut committed = String::new();
        let mut rest = keys;
        while !rest.is_empty() {
            let (key, text, len) = match rest.strip_prefix('<') {
                Some(tail) => {
                    let name = &tail[..tail.find('>').unwrap()];
                    let key = match name {
                        "space" => (Key::Space, " "),
                        "bs" => (Key::Backspace, "\u{7f}"),
                        "esc" => (Key::Escape, "\u{1b}"),
                        "enter" => (Key::Enter, "\r"),
                        "F7" => (Key::Translit(crate::translit::Form::Katakana), ""),
                        other => panic!("unknown key {other}"),
                    };
                    (key.0, key.1.to_string(), name.len() + 2)
                }
                None => {
                    let ch = rest.chars().next().unwrap();
                    (classify(ch), ch.to_string(), ch.len_utf8())
                }
            };
            rest = &rest[len..];
            for effect in session.handle(key, &text).effects {
                if let Effect::Insert(text) = effect {
                    committed.push_str(&text);
                }
            }
        }
        (session.marked_text(), committed)
    }

    // 以下は現挙動の固定 (特性テスト)。期待値は --simulate で得た値で、正しい挙動ではない。
    // 取り消し系は段 3-3 / 3-6 で直す予定。直したらこの期待値を新しい仕様に書き換える。

    #[test]
    fn 現挙動_変換を取り消したあとの英字が確定で落ちる_段3_3で直す予定() {
        let mut s = Session::new(Composer::new(NullEngine));
        assert_eq!(drive(&mut s, "kyouTo<space><bs><bs>"), ("きょうT".to_string(), String::new()));
        // 見た目は きょうT だが、確定は kyouT (変換なしで打った kyouT は きょうT を確定する)
        assert_eq!(drive(&mut s, "<enter>").1, "kyouT");
    }

    #[test]
    fn 現挙動_字種変換を取り消したあとの英字が確定で落ちる_段3_3で直す予定() {
        let mut s = Session::new(Composer::new(NullEngine));
        assert_eq!(drive(&mut s, "kyouTo<F7><esc><bs>"), ("きょうT".to_string(), String::new()));
        assert_eq!(drive(&mut s, "<enter>").1, "kyouT");
    }

    #[test]
    fn 現挙動_取り消し後に打った文字で履歴依存の確定になる_段3_3で直す予定() {
        let mut s = Session::new(Composer::new(NullEngine));
        assert_eq!(drive(&mut s, "iPhone<space><esc>s").0, "いPhones");
        assert_eq!(drive(&mut s, "<enter>").1, "iPhones");
    }

    #[test]
    fn 現挙動_取り消し後のbsと打鍵が英字を読み直す_段3_3で直す予定() {
        let mut s = Session::new(Composer::new(NullEngine));
        assert_eq!(drive(&mut s, "Tokyo<F7><esc><bs>a").0, "Tokyあ");
    }

    #[test]
    fn 現挙動_取り消し後のspaceで英字が変換に入る_段3_3で直す予定() {
        let mut s = Session::new(Composer::new(NullEngine));
        let (shown, committed) = drive(&mut s, "Tokyo<F7><esc><space>");
        assert_eq!((shown.as_str(), committed.as_str()), ("Tokyo", ""));
        assert!(matches!(s.phase(), Phase::Converting { .. }));
    }

    #[test]
    fn 現挙動_英語らしい表示は状態に反映されずxでかなに戻る_段3_6で直す予定() {
        let mut s = english_session();
        assert_eq!(drive(&mut s, "enter").0, "enter");
        assert_eq!(drive(&mut s, "X").0, "えんてrX");
        assert_eq!(drive(&mut s, "<enter>").1, "えんてrX");
    }
}
