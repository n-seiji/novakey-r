#![forbid(unsafe_code)]

// session.rs - 1 キーぶんの判断。ローマ字バッファと composer を持ち、キーを受けて状態を進め、
// Cocoa に対して行うべきこと (Effect) の列を返す。imk.rs は Effect を実行するだけで、ここは headless で動く。

use crate::composer::{CandidateEngine, Committed, Composer, Phase};
use crate::deps::{EnglishMemory, Lexicon};
use crate::keymap::{self, Action, Key};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    SetMarked(String),
    Insert(String),
    /// 候補ウィンドウを出し、選択中の行 (0 起点) をハイライトする。
    ShowCandidates { selected: usize },
    HideCandidates,
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

/// 変換モード。持ち主は Session だけ (実体は composer の live)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConversionMode {
    Live,
    Space,
}

/// 1 回の入力処理で許される外向きの動き。入口で必ず渡す (既定値は持たない。本番では Controller が毎回作る)。
/// 本番では `learning` は `direct` の否定 (outbound::direct_input)。bundle id が取れないときは学習を許可する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gates {
    /// 確定を学習してよい (セキュア入力中・パスワード管理アプリでは false)。
    pub learning: bool,
    /// 変換せず打鍵をそのまま素通しする (セキュア入力中・パスワード管理アプリでは true)。
    pub direct: bool,
}

pub struct Session<E: CandidateEngine, L: Lexicon, M: EnglishMemory> {
    composer: Composer<E, L>,
    memory: M,
    /// アプリで直前に打った文字列の今の行の末尾が ASCII か (`set_context` が更新する)。
    after_ascii: bool,
    /// 直前に打った文字が `<` か (`<-` の `-` を長音にしないため)。
    after_lt: bool,
    /// このセッションが最後に処理したキーが、`<` の直接挿入だったか。
    last_key_was_lt: bool,
}

/// 直前が半角のときに、組成の先頭でも半角のまま入れる記号。`[ ] - / ~` は含めない。
const HALF_WIDTH_AFTER_ASCII: &str = "!?(){}.,:;";

impl<E: CandidateEngine, L: Lexicon, M: EnglishMemory> Session<E, L, M> {
    pub fn new(mut composer: Composer<E, L>, memory: M, mode: ConversionMode) -> Self {
        composer.set_live(mode == ConversionMode::Live);
        Self {
            composer,
            memory,
            after_ascii: false,
            after_lt: false,
            last_key_was_lt: false,
        }
    }

    /// アプリで直前に打った文字列 (空なら文脈なし)。キーごとに、処理の前に渡す。
    /// `english_app` はそのアプリが英語を打つアプリか。true なら、審判は行頭 (空の行・インデントだけ) でも英語の語を聞く。
    /// 変換の文脈になり、今の行 (最後の改行より後ろ) の末尾が ASCII なら組成の先頭の記号を半角のまま入れる。
    pub fn set_context(&mut self, context: &str, english_app: bool) {
        self.composer.engine_mut().set_context(context, english_app);
        self.after_ascii = crate::context::ends_ascii(context);
        self.after_lt = context.ends_with('<');
    }

    pub fn phase(&self) -> Phase {
        self.composer.phase()
    }

    pub fn mode(&self) -> ConversionMode {
        if self.composer.is_live() {
            ConversionMode::Live
        } else {
            ConversionMode::Space
        }
    }

    /// 未確定のローマ字か組成中の文がある。
    pub fn has_pending(&self) -> bool {
        self.composer.has_romaji() || !self.composer.is_empty()
    }

    pub fn candidate_texts(&self) -> Vec<String> {
        self.composer.candidates().iter().map(|c| c.text.clone()).collect()
    }

    /// 変換モードを切り替える。組成中なら表示を新しいモードに合わせ直す。
    pub fn set_live(&mut self, live: bool) -> Vec<Effect> {
        self.composer.set_live(live);
        if self.composer.is_empty() {
            return Vec::new();
        }
        vec![Effect::SetMarked(self.marked_text())]
    }

    /// 候補ウィンドウのハイライト移動 (candidateSelectionChanged:)。選べたときだけ表示を更新する。
    pub fn select_highlight(&mut self, text: &str) -> Vec<Effect> {
        if self.composer.select_candidate_text(text) {
            vec![Effect::SetMarked(self.composer.display().to_string())]
        } else {
            Vec::new()
        }
    }

    /// 下線付きで見せる文字列。
    pub fn marked_text(&self) -> String {
        self.composer.preedit()
    }

    /// いま確定したら挿入される文字列 (状態は変えない)。未確定ローマ字は含めない。
    pub fn commit_preview(&self) -> String {
        self.composer.commit_preview()
    }

    /// 未確定のローマ字があるか。
    pub fn has_romaji(&self) -> bool {
        self.composer.has_romaji()
    }

    /// キー処理の入口。キーを動作表で解決して実行する。
    pub fn handle(&mut self, gates: &Gates, key: Key, text: &str) -> Outcome {
        let after_lt = self.after_lt && self.last_key_was_lt;
        self.last_key_was_lt = false;
        let outcome = self.handle_key(gates, key, text, after_lt);
        self.last_key_was_lt = outcome.action == Some(Action::InsertDirect) && text == "<";
        outcome
    }

    /// セッションを通らないキー (カーソル移動・ショートカット) があった。`<-` の判定をそこで切る。
    pub fn forget_last_key(&mut self) {
        self.last_key_was_lt = false;
    }

    fn handle_key(&mut self, gates: &Gates, key: Key, text: &str, after_lt: bool) -> Outcome {
        let mut fx = Vec::new();
        let phase = self.phase();
        if gates.direct {
            // 直接入力: 作文中だった分 (未確定ローマ字を含む) は表示どおり確定し、キーは素通しにする。
            self.composer.flush_romaji();
            if !self.composer.is_empty() {
                self.commit_and_insert(&Gates { learning: false, ..*gates }, "", &mut fx);
            }
            return Outcome { handled: false, action: Some(Action::PassThrough), phase, effects: fx };
        }
        // `n'` のように、ローマ字表が記号を含む打鍵列は記号キーでも表を優先する。
        let key = match text.chars().collect::<Vec<_>>()[..] {
            [ch] if !ch.is_ascii_alphabetic() && self.composer.accepts_romaji(ch) => {
                Key::Letter { upper: false }
            }
            _ => key,
        };
        if let Some(handled) = self.prepare(key, &mut fx) {
            return Outcome { handled, action: None, phase, effects: fx };
        }
        let phase = self.phase();
        if phase == Phase::Empty && self.after_ascii && matches!(key, Key::Punct | Key::Mark) && text.len() == 1 && HALF_WIDTH_AFTER_ASCII.contains(text) {
            fx.push(Effect::Insert(text.to_string()));
            return Outcome { handled: true, action: Some(Action::InsertDirect), phase, effects: fx };
        }
        // `<` を打った直後の `-` は長音にせず半角のまま入れる (`<-`)。
        if phase == Phase::Empty && after_lt && key == Key::Hyphen {
            fx.push(Effect::Insert(text.to_string()));
            return Outcome { handled: true, action: Some(Action::InsertDirect), phase, effects: fx };
        }
        let action = keymap::action_for(phase, key);
        if action == Action::CommitThenRestart {
            self.commit_and_insert(gates, "", &mut fx);
            let restarted = self.handle(gates, key, text);
            fx.extend(restarted.effects);
            return Outcome { handled: restarted.handled, action: Some(action), phase, effects: fx };
        }
        let handled = self.perform(gates, action, text, &mut fx);
        Outcome { handled, action: Some(action), phase, effects: fx }
    }

    /// 候補ウィンドウでの選択を確定する (candidateSelected:)。
    pub fn commit_selected(&mut self, gates: &Gates, text: &str) -> Vec<Effect> {
        self.composer.select_candidate_text(text);
        let mut fx = Vec::new();
        self.commit_and_insert(gates, "", &mut fx);
        fx
    }

    /// 入力ソースの切り替えなどによる確定 (commitComposition:)。
    pub fn commit_composition(&mut self, gates: &Gates) -> Vec<Effect> {
        self.last_key_was_lt = false;
        let mut fx = Vec::new();
        self.composer.flush_romaji();
        if !self.composer.is_empty() {
            // フォーカス喪失などによる確定は、英語を「選んだ」のではないので、選択による学習はしない。
            fx.push(Effect::Insert(self.commit(gates, false)));
        }
        fx.push(Effect::HideCandidates);
        fx
    }

    /// 確定して、学習してよければ学習する。学習の可否を判定するのはここだけ。
    /// 学習しない (ゲートが閉じている / 学習対象の engine 候補が無い) ときは engine の保留を取り消す。
    /// chosen は、利用者が確定を選んだ (commitComposition ではない) とき true。候補から選んだ英字はそのときだけ学習する。
    fn commit(&mut self, gates: &Gates, chosen: bool) -> String {
        let Committed { text, engine, english, picked } = self.composer.commit();
        match engine.filter(|_| gates.learning) {
            Some((generation, rank)) => {
                let _ = self.composer.engine_mut().select(generation, rank.0);
            }
            None => self.composer.engine_mut().cancel(),
        }
        if gates.learning {
            for word in english.iter().chain(picked.iter().filter(|_| chosen)) {
                self.memory.learn(word);
            }
        }
        text
    }

    fn refresh_marked(&self, fx: &mut Vec<Effect>) {
        fx.push(Effect::SetMarked(self.marked_text()));
    }

    /// 現在の表示内容を確定して挿入する。suffix は確定直後に続けて入れる文字列。
    fn commit_and_insert(&mut self, gates: &Gates, suffix: &str, fx: &mut Vec<Effect>) {
        let text = self.commit(gates, true);
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
            self.composer.flush_romaji();
        }
        let has_romaji = self.composer.has_romaji();
        match key {
            Key::Backspace if has_romaji => {
                self.composer.backspace_romaji();
                self.refresh_marked(fx);
                Some(true)
            }
            Key::Escape => {
                self.composer.clear_romaji();
                if has_romaji && self.composer.is_empty() {
                    fx.push(Effect::SetMarked(String::new()));
                    return Some(true);
                }
                None
            }
            _ => None,
        }
    }

    fn perform(&mut self, gates: &Gates, action: Action, text: &str, fx: &mut Vec<Effect>) -> bool {
        match action {
            Action::PassThrough => false,
            Action::InsertSpace => {
                fx.push(Effect::Insert(" ".to_string()));
                true
            }
            Action::InsertDirect => {
                fx.push(Effect::Insert(text.to_string()));
                true
            }
            Action::TypeRomaji => {
                self.composer.type_romaji(text);
                self.show_typing(fx)
            }
            Action::StartLiteral | Action::TypeLiteral => {
                self.composer.push_literal(text);
                self.show_typing(fx)
            }
            Action::TypeText => {
                self.composer.type_text(text);
                self.show_typing(fx)
            }
            Action::Convert => {
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
                self.commit_and_insert(gates, "", fx);
                false
            }
            Action::PickCandidate(number) => {
                // 候補が 9 件を超えてパネルがスクロールした後は、行番号と数字の対応が分からないので何もしない。
                if self.composer.digits_pick() && self.composer.select_candidate(usize::from(number) - 1) {
                    self.commit_and_insert(gates, "", fx);
                }
                true
            }
            Action::Commit => {
                self.commit_and_insert(gates, "", fx);
                true
            }
            Action::CommitThenSpace => {
                self.commit_and_insert(gates, " ", fx);
                true
            }
            Action::CommitThenInsert => {
                self.commit_and_insert(gates, text, fx);
                true
            }
            // handle が先に処理するのでここには来ない。
            Action::CommitThenRestart => false,
            Action::Clear => {
                self.composer.discard();
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
                    self.show_typing(fx);
                }
                deleted
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::deps::{FakeLexicon, FakeMemory};
    use crate::keymap::classify;

    const ALLOW: Gates = Gates { learning: true, direct: false };

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
        let mut session = Session::new(Composer::new(NullEngine, FakeLexicon::default()), FakeMemory::default(), ConversionMode::Space);
        let mut out = String::new();
        for ch in keys.chars().chain(['\r']) {
            let outcome = session.handle(&ALLOW, classify(ch), &ch.to_string());
            for effect in outcome.effects {
                if let Effect::Insert(text) = effect {
                    out.push_str(&text);
                }
            }
        }
        out
    }

    fn english_session() -> Session<NullEngine, FakeLexicon, FakeMemory> {
        let lexicon = FakeLexicon::default().with_english_word(|w| matches!(w, "enter" | "rain" | "in"));
        let memory = lexicon.memory();
        Session::new(Composer::new(NullEngine, lexicon), memory, ConversionMode::Space)
    }

    /// 打鍵列を流し、各打鍵後の表示と、最後に Enter で確定した文字列を返す。
    fn typed_english(keys: &str) -> (Vec<String>, String) {
        let mut session = english_session();
        let mut shown = Vec::new();
        let mut out = String::new();
        for ch in keys.chars().chain(['\r']) {
            for effect in session.handle(&ALLOW, classify(ch), &ch.to_string()).effects {
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

    fn remembering_session() -> Session<NullEngine, FakeLexicon, FakeMemory> {
        let lexicon = FakeLexicon::default();
        let memory = lexicon.memory();
        Session::new(Composer::new(NullEngine, lexicon), memory, ConversionMode::Space)
    }

    fn shown_after(session: &mut Session<NullEngine, FakeLexicon, FakeMemory>, keys: &str) -> String {
        for ch in keys.chars() {
            session.handle(&ALLOW, classify(ch), &ch.to_string());
        }
        session.marked_text()
    }

    #[test]
    fn 英語として確定した語は次から打ちかけでも英字で表示する() {
        let mut session = remembering_session();
        assert_eq!(shown_after(&mut session, "git"), "ぎt");
        session.handle(&ALLOW, Key::Enter, "\r");
        assert_eq!(shown_after(&mut session, "git"), "git");
        assert_eq!(shown_after(&mut session, "a"), "ぎた");
    }

    #[test]
    fn かなで確定した語は覚えない() {
        let mut session = remembering_session();
        shown_after(&mut session, "gita");
        session.handle(&ALLOW, Key::Enter, "\r");
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

    fn shown_after_english(session: &mut Session<NullEngine, FakeLexicon, FakeMemory>, keys: &str) {
        for ch in keys.chars() {
            session.handle(&ALLOW, classify(ch), &ch.to_string());
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
            session.handle(&ALLOW, classify(ch), &ch.to_string());
        }
        session.handle(&ALLOW, Key::ShiftSpace, " ");
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
        let mut session = Session::new(Composer::new(NullEngine, FakeLexicon::default()), FakeMemory::default(), ConversionMode::Space);
        let (mut committed, mut marked) = (String::new(), String::new());
        for ch in keys.chars() {
            let outcome = session.handle(&ALLOW, classify(ch), &ch.to_string());
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

    fn effects_of(session: &mut Session<TwoEngine, FakeLexicon, FakeMemory>, keys: &str) -> Vec<Effect> {
        effects_with(&ALLOW, session, keys)
    }

    fn effects_with(gates: &Gates, session: &mut Session<TwoEngine, FakeLexicon, FakeMemory>, keys: &str) -> Vec<Effect> {
        keys.chars()
            .flat_map(|ch| session.handle(gates, classify(ch), &ch.to_string()).effects)
            .collect()
    }

    #[test]
    fn 作った時のモードを返し_set_liveで組成中の表示が作り直される() {
        let mut session = Session::new(Composer::new(TwoEngine, FakeLexicon::default()), FakeMemory::default(), ConversionMode::Live);
        assert_eq!(session.mode(), ConversionMode::Live);
        assert!(session.set_live(false).is_empty(), "組成が空なら表示は触らない");
        assert_eq!(session.mode(), ConversionMode::Space);
        effects_of(&mut session, "kyou");
        let fx = session.set_live(true);
        assert_eq!(session.mode(), ConversionMode::Live);
        assert_eq!(fx, vec![Effect::SetMarked(session.marked_text())]);
    }

    #[test]
    fn has_pendingとcandidate_textsは組成の状態を映す() {
        let mut session = Session::new(Composer::new(TwoEngine, FakeLexicon::default()), FakeMemory::default(), ConversionMode::Space);
        assert!(!session.has_pending());
        effects_of(&mut session, "ky");
        assert!(session.has_pending(), "未変換のローマ字だけでも保留中");
        effects_of(&mut session, "ou ");
        assert_eq!(session.candidate_texts().last().map(String::as_str), Some("kyou"), "英単語でない raw は末尾");
    }

    #[test]
    fn select_highlightは候補にあるときだけ表示を更新する() {
        let mut session = Session::new(Composer::new(TwoEngine, FakeLexicon::default()), FakeMemory::default(), ConversionMode::Space);
        effects_of(&mut session, "kyou ");
        assert!(session.select_highlight("無い").is_empty());
        assert_eq!(session.select_highlight("x"), vec![Effect::SetMarked("x".to_string())]);
    }

    /// `<space>` `<bs>` `<esc>` `<enter>` `<F7>` を含む打鍵列を流し、(最後の表示, 確定した文字列) を返す。
    fn drive(session: &mut Session<NullEngine, FakeLexicon, FakeMemory>, keys: &str) -> (String, String) {
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
            for effect in session.handle(&ALLOW, key, &text).effects {
                if let Effect::Insert(text) = effect {
                    committed.push_str(&text);
                }
            }
        }
        (session.marked_text(), committed)
    }

    // 取り消しは英字入力 (Literal / Mixed) の局面を保つ。表示と確定は一致する。

    #[test]
    fn 変換を取り消したあとも英字が確定に残る() {
        let mut s = Session::new(Composer::new(NullEngine, FakeLexicon::default()), FakeMemory::default(), ConversionMode::Space);
        assert_eq!(drive(&mut s, "kyouTo<space><bs><bs>"), ("きょうT".to_string(), String::new()));
        assert_eq!(drive(&mut s, "<enter>").1, "きょうT");
    }

    #[test]
    fn 字種変換を取り消したあとも英字が確定に残る() {
        let mut s = Session::new(Composer::new(NullEngine, FakeLexicon::default()), FakeMemory::default(), ConversionMode::Space);
        assert_eq!(drive(&mut s, "kyouTo<F7><esc><bs>"), ("きょうT".to_string(), String::new()));
        assert_eq!(drive(&mut s, "<enter>").1, "きょうT");
    }

    #[test]
    fn 取り消し後に打った文字も変換なしと同じ確定になる() {
        let mut s = Session::new(Composer::new(NullEngine, FakeLexicon::default()), FakeMemory::default(), ConversionMode::Space);
        assert_eq!(drive(&mut s, "iPhone<space><esc>s").0, "いPhones");
        assert_eq!(drive(&mut s, "<enter>").1, "いPhones");
    }

    #[test]
    fn 取り消し後のbsと打鍵は英字のまま続く() {
        let mut s = Session::new(Composer::new(NullEngine, FakeLexicon::default()), FakeMemory::default(), ConversionMode::Space);
        assert_eq!(drive(&mut s, "Tokyo<F7><esc><bs>a").0, "Tokya");
    }

    #[test]
    fn 取り消し後のspaceは変換せず英字を確定して空白を入れる() {
        let mut s = Session::new(Composer::new(NullEngine, FakeLexicon::default()), FakeMemory::default(), ConversionMode::Space);
        let (shown, committed) = drive(&mut s, "Tokyo<F7><esc><space>");
        assert_eq!((shown.as_str(), committed.as_str()), ("", "Tokyo "));
        assert_eq!(s.phase(), Phase::Empty);
    }

    // 英語分割 (artifactni) は変換に入るときに英単語とかなへ分ける。取り消しは分ける前の打鍵中に戻る。

    #[test]
    fn 英語分割の変換を取り消して続きを打つと取り消さずに打った結果と同じ() {
        let mut s = english_session();
        let reverted = drive(&mut s, "artifactni<space><esc>tsuite<enter>");
        let mut d = english_session();
        let direct = drive(&mut d, "artifactnitsuite<enter>");
        assert_eq!(reverted, direct);
        assert_eq!(reverted.1, "artifactについて");
    }

    #[test]
    fn 英語分割の変換を取り消してbsしても取り消さずに打った結果と同じ() {
        let mut s = english_session();
        let reverted = drive(&mut s, "artifactni<space><esc><bs><space>");
        let mut d = english_session();
        let direct = drive(&mut d, "artifactni<bs><space>");
        assert_eq!(reverted, direct);
        assert_eq!(s.phase(), d.phase());
    }

    #[test]
    fn かなを消しきった後のbsと打鍵は英字のまま続く() {
        let mut s = Session::new(Composer::new(NullEngine, FakeLexicon::default()), FakeMemory::default(), ConversionMode::Space);
        assert_eq!(drive(&mut s, "Tokyo<bs>a").0, "Tokya");
        assert_eq!(s.phase(), Phase::Literal);
    }

    #[test]
    fn 英語らしい表示になったあと大文字を続けても表示どおりに確定する() {
        let mut s = english_session();
        assert_eq!(drive(&mut s, "enter").0, "enter");
        assert_eq!(drive(&mut s, "X").0, "enterX");
        assert_eq!(drive(&mut s, "<enter>").1, "enterX");
        assert_eq!(drive(&mut english_session(), "mergeX<enter>").1, "mergeX");
    }

    #[derive(Clone, Default)]
    struct RecordingEngine {
        selects: std::rc::Rc<std::cell::RefCell<Vec<(u64, usize)>>>,
        cancels: std::rc::Rc<std::cell::Cell<usize>>,
    }
    impl CandidateEngine for RecordingEngine {
        fn convert(&mut self, _: u64, hiragana: &str) -> Vec<String> {
            vec![hiragana.to_string(), "x".to_string()]
        }
        fn select(&mut self, revision: u64, index: usize) -> bool {
            self.selects.borrow_mut().push((revision, index));
            true
        }
        fn cancel(&mut self) {
            self.cancels.set(self.cancels.get() + 1);
        }
    }

    /// keys を打ち、Enter で確定する。engine と英語の記憶の両方を返す。
    fn commit_with(gates: &Gates, keys: &str) -> (RecordingEngine, FakeMemory) {
        let engine = RecordingEngine::default();
        let lexicon = FakeLexicon::default();
        let memory = lexicon.memory();
        let mut session = Session::new(Composer::new(engine.clone(), lexicon), memory.clone(), ConversionMode::Space);
        for ch in keys.chars() {
            session.handle(gates, classify(ch), &ch.to_string());
        }
        session.handle(gates, Key::Enter, "\r");
        (engine, memory)
    }

    #[test]
    fn 学習が許されれば確定した候補をengineに学習させる() {
        let (engine, _) = commit_with(&ALLOW, "kyou ");
        assert_eq!(engine.selects.borrow().len(), 1);
        assert_eq!(engine.cancels.get(), 0);
    }

    #[test]
    fn 学習が許されなければengineの保留を取り消して学習しない() {
        let denied = Gates { learning: false, direct: false };
        let (engine, memory) = commit_with(&denied, "kyou ");
        assert!(engine.selects.borrow().is_empty());
        assert_eq!(engine.cancels.get(), 1);
        assert!(memory.learned().is_empty());
    }

    #[test]
    fn engine候補でない確定ではengineを取り消す() {
        let (engine, _) = commit_with(&ALLOW, "kyou   ");
        assert!(engine.selects.borrow().is_empty(), "raw 候補は学習しない");
        assert_eq!(engine.cancels.get(), 1);
    }

    #[test]
    fn 英字で確定した語は学習が許されるときだけ覚える() {
        assert_eq!(commit_with(&ALLOW, "G").1.learned(), ["G"]);
        let denied = Gates { learning: false, direct: false };
        assert!(commit_with(&denied, "G").1.learned().is_empty());
    }

    /// どの読みも辞書の見出しとして答える RecordingEngine (done が どね の見出しと衝突して、Space で候補が出る)。
    struct HeadwordEngine(RecordingEngine);
    impl CandidateEngine for HeadwordEngine {
        fn convert(&mut self, revision: u64, hiragana: &str) -> Vec<String> {
            self.0.convert(revision, hiragana)
        }
        fn select(&mut self, revision: u64, index: usize) -> bool {
            self.0.select(revision, index)
        }
        fn cancel(&mut self) {
            self.0.cancel();
        }
        fn has_reading(&self, _: &str) -> bool {
            true
        }
    }

    /// done を辞書の英単語とし、done を打って Space で候補を出し、done を選ぶ。finish が確定のしかた。
    fn pick_done(gates: &Gates, finish: impl FnOnce(&mut Session<HeadwordEngine, FakeLexicon, FakeMemory>, &Gates)) -> FakeMemory {
        let lexicon = FakeLexicon::default().with_english_word(|w| w == "done");
        let memory = lexicon.memory();
        let mut session = Session::new(Composer::new(HeadwordEngine(RecordingEngine::default()), lexicon), memory.clone(), ConversionMode::Space);
        for ch in "done ".chars() {
            session.handle(gates, classify(ch), &ch.to_string());
        }
        assert!(session.composer.select_candidate_text("done"));
        finish(&mut session, gates);
        memory
    }

    #[test]
    fn 候補から選んだ英字は学習が許されるときだけ英語として覚える() {
        let enter = |s: &mut Session<_, _, _>, g: &Gates| {
            s.handle(g, Key::Enter, "\r");
        };
        assert_eq!(pick_done(&ALLOW, enter).learned(), ["done"]);
        assert_eq!(pick_done(&ALLOW, |s, g| drop(s.commit_selected(g, "done"))).learned(), ["done"]);
        let denied = Gates { learning: false, direct: false };
        assert!(pick_done(&denied, enter).learned().is_empty());
    }

    /// done の raw 候補の 1 始まりの番号。
    fn done_number(session: &Session<HeadwordEngine, FakeLexicon, FakeMemory>) -> char {
        let index = session.composer.candidates().iter().position(|c| c.text == "done").expect("done is a candidate");
        char::from_digit(index as u32 + 1, 10).expect("done is within the digit picks")
    }

    /// done を打って Space で候補を出し、done に移ってから keys を打つ (選択の確定は keys に任せる)。
    fn pick_done_with_keys(gates: &Gates, pick: impl FnOnce(&mut Session<HeadwordEngine, FakeLexicon, FakeMemory>, &Gates), keys: &str) -> FakeMemory {
        pick_done(gates, |s, g| {
            pick(s, g);
            for ch in keys.chars() {
                s.handle(g, classify(ch), &ch.to_string());
            }
        })
    }

    #[test]
    fn 数字キーで選んだ英字も学習が許されるときだけ英語として覚える() {
        // 候補ウィンドウが開いている (pick_done が done を選んだ) 状態から、先頭へ戻して done の番号キーで選び直す。
        let by_digit = |s: &mut Session<_, _, _>, g: &Gates| {
            assert!(s.composer.select_candidate(0));
            let number = done_number(s);
            s.handle(g, classify(number), &number.to_string());
        };
        assert_eq!(pick_done(&ALLOW, by_digit).learned(), ["done"]);
        let denied = Gates { learning: false, direct: false };
        assert!(pick_done(&denied, by_digit).learned().is_empty());
    }

    #[test]
    fn 英字を選んだまま文字や記号を打って確定しても英語として覚える() {
        for keys in ["a", "."] {
            assert_eq!(pick_done_with_keys(&ALLOW, |_, _| {}, keys).learned(), ["done"], "{keys}");
        }
    }

    #[test]
    fn 直接入力では英字を選んでいても覚えない() {
        let direct = Gates { learning: true, direct: true };
        let memory = pick_done(&ALLOW, |s, _| {
            s.handle(&direct, classify('a'), "a");
        });
        assert!(memory.learned().is_empty());
    }

    #[test]
    fn 入力ソース切り替えなどの確定では英字を選んだことにしない() {
        let memory = pick_done(&ALLOW, |s, g| drop(s.commit_composition(g)));
        assert!(memory.learned().is_empty());
    }

    fn inserted(outcome: &Outcome) -> Vec<&str> {
        outcome.effects.iter().filter_map(|e| if let Effect::Insert(t) = e { Some(t.as_str()) } else { None }).collect()
    }

    #[test]
    fn 直前が_ascii_なら組成の先頭の記号を半角のまま入れる() {
        for ch in "!?(){}.,:;".chars() {
            let mut session = Session::new(Composer::new(NullEngine, FakeLexicon::default()), FakeMemory::default(), ConversionMode::Space);
            session.set_context("hello", false);
            let outcome = session.handle(&ALLOW, classify(ch), &ch.to_string());
            assert!(outcome.handled, "{ch}");
            assert_eq!(inserted(&outcome), [ch.to_string()], "{ch}");
            assert_eq!(session.phase(), Phase::Empty, "{ch}");
        }
    }

    #[test]
    fn 文脈が無いか日本語で終わるときは記号を今までどおり全角にする() {
        for context in ["", "   ", "今日は", "hello。"] {
            let mut session = Session::new(Composer::new(NullEngine, FakeLexicon::default()), FakeMemory::default(), ConversionMode::Space);
            session.set_context(context, false);
            let outcome = session.handle(&ALLOW, classify('!'), "!");
            assert!(inserted(&outcome).is_empty(), "{context:?}");
            assert_eq!(session.marked_text(), "！", "{context:?}");
        }
    }

    #[test]
    fn 直前が_ascii_でも_括弧とハイフンなどは日本語のまま() {
        for (ch, want) in [('[', "「"), (']', "」"), ('-', "ー"), ('/', "・"), ('~', "〜")] {
            let mut session = Session::new(Composer::new(NullEngine, FakeLexicon::default()), FakeMemory::default(), ConversionMode::Space);
            session.set_context("hello", false);
            session.handle(&ALLOW, classify(ch), &ch.to_string());
            assert_eq!(session.marked_text(), want, "{ch}");
        }
    }

    #[test]
    fn 組成の途中の記号は文脈に関わらず今までどおり() {
        let mut session = Session::new(Composer::new(NullEngine, FakeLexicon::default()), FakeMemory::default(), ConversionMode::Space);
        session.set_context("hello", false);
        session.handle(&ALLOW, classify('a'), "a");
        session.handle(&ALLOW, classify('!'), "!");
        assert_eq!(session.marked_text(), "あ！");
    }

    #[test]
    fn ハイフンの直後の_gt_は長音をやめて矢印にする() {
        let mut session = Session::new(Composer::new(NullEngine, FakeLexicon::default()), FakeMemory::default(), ConversionMode::Space);
        session.handle(&ALLOW, classify('-'), "-");
        assert_eq!(session.marked_text(), "ー");
        session.handle(&ALLOW, classify('>'), ">");
        assert_eq!(session.marked_text(), "->");
        assert_eq!(session.commit_preview(), "->");
    }

    #[test]
    fn lt_の直後のハイフンは長音にしない() {
        let mut session = Session::new(Composer::new(NullEngine, FakeLexicon::default()), FakeMemory::default(), ConversionMode::Space);
        session.handle(&ALLOW, classify('<'), "<");
        session.set_context("<", false);
        let outcome = session.handle(&ALLOW, classify('-'), "-");
        assert_eq!(inserted(&outcome), ["-"]);
        assert_eq!(session.marked_text(), "");
    }

    #[test]
    fn lt_のあとにカーソルが動いたら_ハイフンは長音のまま() {
        let mut session = Session::new(Composer::new(NullEngine, FakeLexicon::default()), FakeMemory::default(), ConversionMode::Space);
        session.handle(&ALLOW, classify('<'), "<");
        session.forget_last_key();
        session.set_context("<", false);
        let outcome = session.handle(&ALLOW, classify('-'), "-");
        assert!(inserted(&outcome).is_empty());
        assert_eq!(session.marked_text(), "ー");
    }

    #[test]
    fn 矢印のgtを_bs_で消すと長音に戻る() {
        let mut session = Session::new(Composer::new(NullEngine, FakeLexicon::default()), FakeMemory::default(), ConversionMode::Space);
        session.handle(&ALLOW, classify('-'), "-");
        session.handle(&ALLOW, classify('>'), ">");
        session.handle(&ALLOW, Key::Backspace, "\u{7f}");
        assert_eq!(session.marked_text(), "ー");
    }
}
