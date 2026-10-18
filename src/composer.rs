#![forbid(unsafe_code)]

// composer.rs - 未確定文の状態機械: 入力単位(ローマ字+かな) + 同期変換候補 + フェーズ。
// 候補は {text, engine_index} で持ち、リランキング後も学習には元 index を渡す。
// ローマ字そのまま(raw)の候補は engine_index を持たず、確定しても学習しない。

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub text: String,
    pub engine_index: Option<usize>,
}

pub trait CandidateEngine {
    fn convert(&mut self, revision: u64, hiragana: &str) -> Vec<String>;
    fn select(&mut self, revision: u64, engine_index: usize) -> bool;
    fn cancel(&mut self);
}

/// 未確定文の局面。Empty は未確定文が無い状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Empty,
    /// かなを入力中(未変換)。
    Composing,
    /// 変換を取り消して、かなをそのまま表示している。
    Reverted,
    /// 候補を選択中。window は候補ウィンドウを出している間 true。
    Converting { selected: usize, window: bool },
    /// 英単語をそのまま入力中(かな変換しない)。
    Literal,
}

/// 1 回の入力で生まれた (打鍵, かな) の組。部分削除後は raw をかなで置き換える。
#[derive(Debug, Clone)]
struct Unit {
    raw: String,
    kana: String,
}

pub struct Composer<E: CandidateEngine> {
    engine: E,
    units: Vec<Unit>,
    pending_raw: String,
    hiragana: String,
    candidates: Vec<Candidate>,
    phase: Phase,
    live: bool,
    generation: u64,
    pending_rerank: Option<usize>,
}

impl<E: CandidateEngine> Composer<E> {
    pub fn new(engine: E) -> Self {
        Self {
            engine,
            units: Vec::new(),
            pending_raw: String::new(),
            hiragana: String::new(),
            candidates: Vec::new(),
            phase: Phase::Empty,
            live: false,
            generation: 0,
            pending_rerank: None,
        }
    }

    pub fn set_live(&mut self, live: bool) {
        self.live = live;
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn is_empty(&self) -> bool {
        self.hiragana.is_empty()
    }

    pub fn current_generation(&self) -> u64 {
        self.generation
    }

    pub fn candidates(&self) -> &[Candidate] {
        &self.candidates
    }

    pub fn display_hiragana(&self) -> &str {
        &self.hiragana
    }

    /// 打鍵したままの ASCII 文字列(かなに変換済みの分と未変換の分)。
    pub fn raw(&self) -> String {
        self.units
            .iter()
            .map(|u| u.raw.as_str())
            .chain(std::iter::once(self.pending_raw.as_str()))
            .collect()
    }

    pub fn display(&self) -> &str {
        match self.phase {
            Phase::Converting { selected, .. } => &self.candidates[selected].text,
            Phase::Composing if self.live => self
                .candidates
                .first()
                .map_or(self.hiragana.as_str(), |c| c.text.as_str()),
            _ => &self.hiragana,
        }
    }

    fn sync_hiragana(&mut self) {
        self.hiragana = self.units.iter().map(|u| u.kana.as_str()).collect();
    }

    fn reset_candidates(&mut self) {
        self.generation += 1;
        self.candidates.clear();
        self.pending_rerank = None;
    }

    fn refresh(&mut self) {
        self.reset_candidates();
        self.sync_hiragana();
        if self.hiragana.is_empty() {
            self.phase = Phase::Empty;
            return;
        }
        self.phase = Phase::Composing;
        let texts = self.engine.convert(self.generation, &self.hiragana);
        for (engine_index, text) in texts.into_iter().enumerate() {
            if !self.candidates.iter().any(|c| c.text == text) {
                self.candidates.push(Candidate {
                    text,
                    engine_index: Some(engine_index),
                });
            }
        }
    }

    /// 打鍵 raw と、それで確定したかな kana を追加する。kana が空なら raw は次のかなに持ち越す。
    pub fn push_input(&mut self, raw: &str, kana: &str) {
        self.pending_raw.push_str(raw);
        if kana.is_empty() {
            return;
        }
        self.units.push(Unit {
            raw: std::mem::take(&mut self.pending_raw),
            kana: kana.to_string(),
        });
        self.refresh();
    }

    /// 英単語としてそのまま追加する。かな変換も候補生成もしない。
    pub fn push_literal(&mut self, text: &str) {
        if !self.candidates.is_empty() {
            self.engine.cancel();
        }
        let raw = format!("{}{}", std::mem::take(&mut self.pending_raw), text);
        self.units.push(Unit {
            raw,
            kana: text.to_string(),
        });
        self.sync_hiragana();
        self.reset_candidates();
        self.phase = Phase::Literal;
    }

    pub fn pop_pending_raw(&mut self) {
        self.pending_raw.pop();
    }

    pub fn clear_pending_raw(&mut self) {
        self.pending_raw.clear();
    }

    pub fn backspace(&mut self) -> bool {
        let Some(last) = self.units.last_mut() else {
            return false;
        };
        last.kana.pop();
        if last.kana.is_empty() {
            self.units.pop();
        } else {
            last.raw = last.kana.clone();
        }
        if self.phase == Phase::Literal && !self.units.is_empty() {
            self.sync_hiragana();
            self.reset_candidates();
        } else {
            self.refresh();
        }
        true
    }

    /// 最初の変換。保留中のリランキングを反映し、raw 候補を第 1 候補の直後に入れて先頭を選ぶ。
    pub fn begin_conversion(&mut self) -> bool {
        if !matches!(self.phase, Phase::Composing | Phase::Reverted) {
            return false;
        }
        if let Some(index) = self.pending_rerank.take() {
            self.promote(index);
        }
        self.insert_raw_candidate();
        if self.candidates.is_empty() {
            return false;
        }
        self.phase = Phase::Converting {
            selected: 0,
            window: false,
        };
        true
    }

    /// 次の候補へ進み(末尾なら先頭へ戻り)、候補ウィンドウを出す状態にする。
    pub fn advance(&mut self) -> bool {
        let Phase::Converting { selected, .. } = self.phase else {
            return false;
        };
        self.phase = Phase::Converting {
            selected: (selected + 1) % self.candidates.len(),
            window: true,
        };
        true
    }

    pub fn select_candidate(&mut self, index: usize) -> bool {
        if index >= self.candidates.len() {
            return false;
        }
        self.phase = Phase::Converting {
            selected: index,
            window: true,
        };
        true
    }

    pub fn select_candidate_text(&mut self, text: &str) -> bool {
        match self.candidates.iter().position(|c| c.text == text) {
            Some(index) => self.select_candidate(index),
            None => false,
        }
    }

    pub fn revert(&mut self) {
        if self.phase != Phase::Empty {
            self.phase = Phase::Reverted;
        }
    }

    pub fn commit(&mut self) -> String {
        let out = self.display().to_string();
        let learned = match self.phase {
            Phase::Converting { selected, .. } => self.candidates[selected].engine_index,
            Phase::Composing if self.live => self.candidates.first().and_then(|c| c.engine_index),
            _ => None,
        };
        match learned {
            Some(i) => {
                let _ = self.engine.select(self.generation, i);
            }
            None => self.engine.cancel(),
        }
        self.units.clear();
        self.pending_raw.clear();
        self.hiragana.clear();
        self.reset_candidates();
        self.phase = Phase::Empty;
        out
    }

    /// LLM が選んだ表示位置 display_index を先頭へ昇格する。
    /// live なら即反映して新しい表示を返す。非 live では保留し、最初の変換時に反映する。
    pub fn apply_rerank(&mut self, generation: u64, display_index: usize) -> Option<String> {
        if generation != self.generation
            || self.phase != Phase::Composing
            || display_index == 0
            || display_index >= self.candidates.len()
        {
            return None;
        }
        if self.live {
            self.promote(display_index);
            Some(self.display().to_string())
        } else {
            self.pending_rerank = Some(display_index);
            None
        }
    }

    fn promote(&mut self, display_index: usize) {
        if display_index < self.candidates.len() {
            let chosen = self.candidates.remove(display_index);
            self.candidates.insert(0, chosen);
        }
    }

    fn insert_raw_candidate(&mut self) {
        let raw = self.raw();
        if raw.is_empty() || self.candidates.iter().any(|c| c.text == raw) {
            return;
        }
        let position = self.candidates.len().min(1);
        self.candidates.insert(
            position,
            Candidate {
                text: raw,
                engine_index: None,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Clone, Default)]
    struct Log {
        converts: Rc<RefCell<Vec<(u64, String)>>>,
        selects: Rc<RefCell<Vec<(u64, usize)>>>,
        cancels: Rc<RefCell<usize>>,
    }

    struct MockEngine {
        log: Log,
        table: Vec<(String, Vec<String>)>,
    }

    impl CandidateEngine for MockEngine {
        fn convert(&mut self, revision: u64, hiragana: &str) -> Vec<String> {
            self.log
                .converts
                .borrow_mut()
                .push((revision, hiragana.to_string()));
            for (k, v) in &self.table {
                if k == hiragana {
                    return v.clone();
                }
            }
            vec![format!("{}K1", hiragana), format!("{}K2", hiragana)]
        }
        fn select(&mut self, revision: u64, engine_index: usize) -> bool {
            self.log.selects.borrow_mut().push((revision, engine_index));
            true
        }
        fn cancel(&mut self) {
            *self.log.cancels.borrow_mut() += 1;
        }
    }

    fn composer() -> (Composer<MockEngine>, Log) {
        let log = Log::default();
        let engine = MockEngine {
            log: log.clone(),
            table: vec![(
                "きょう".to_string(),
                vec!["今日".to_string(), "京".to_string(), "教".to_string()],
            )],
        };
        (Composer::new(engine), log)
    }

    fn type_kyou(c: &mut Composer<MockEngine>) {
        c.push_input("k", "");
        c.push_input("yo", "きょ");
        c.push_input("u", "う");
    }

    fn texts(c: &Composer<MockEngine>) -> Vec<&str> {
        c.candidates().iter().map(|x| x.text.as_str()).collect()
    }

    #[test]
    fn 入力なしでは空() {
        let (c, _) = composer();
        assert!(c.is_empty());
        assert_eq!(c.phase(), Phase::Empty);
        assert_eq!(c.display(), "");
    }

    #[test]
    fn 既定は非liveで生ひらがなを表示() {
        let (mut c, log) = composer();
        type_kyou(&mut c);
        assert_eq!(c.phase(), Phase::Composing);
        assert_eq!(c.display(), "きょう");
        assert_eq!(c.raw(), "kyou");
        assert_eq!(log.converts.borrow().len(), 2);
    }

    #[test]
    fn liveなら第1候補を表示() {
        let (mut c, _) = composer();
        c.set_live(true);
        type_kyou(&mut c);
        assert_eq!(c.display(), "今日");
    }

    #[test]
    fn 最初の変換で第1候補を選び_候補ウィンドウはまだ出さない() {
        let (mut c, _) = composer();
        type_kyou(&mut c);
        assert!(c.begin_conversion());
        assert_eq!(
            c.phase(),
            Phase::Converting {
                selected: 0,
                window: false
            }
        );
        assert_eq!(c.display(), "今日");
    }

    #[test]
    fn 変換後の送りで候補を順に進めwrapし_ウィンドウを出す() {
        let (mut c, _) = composer();
        type_kyou(&mut c);
        c.begin_conversion();
        assert!(c.advance());
        assert_eq!(
            c.phase(),
            Phase::Converting {
                selected: 1,
                window: true
            }
        );
        assert_eq!(c.display(), "kyou");
        c.advance();
        c.advance();
        assert_eq!(c.display(), "教");
        c.advance();
        assert_eq!(c.display(), "今日");
    }

    #[test]
    fn 変換前は送れない() {
        let (mut c, _) = composer();
        assert!(!c.begin_conversion());
        assert!(!c.advance());
        type_kyou(&mut c);
        assert!(!c.advance());
    }

    #[test]
    fn raw候補は第1候補の直後に一度だけ入る() {
        let (mut c, _) = composer();
        type_kyou(&mut c);
        c.begin_conversion();
        assert_eq!(texts(&c), vec!["今日", "kyou", "京", "教"]);
        c.revert();
        assert!(c.begin_conversion());
        assert_eq!(texts(&c), vec!["今日", "kyou", "京", "教"]);
    }

    #[test]
    fn raw候補は同じ文字列の候補があれば重複しない() {
        let log = Log::default();
        let engine = MockEngine {
            log,
            table: vec![("ぎっとはぶ".to_string(), vec!["github".to_string()])],
        };
        let mut c = Composer::new(engine);
        c.push_input("github", "ぎっとはぶ");
        c.begin_conversion();
        assert_eq!(texts(&c), vec!["github"]);
        assert_eq!(c.candidates()[0].engine_index, Some(0));
    }

    #[test]
    fn 変換候補が空でもraw候補だけで変換できる() {
        let log = Log::default();
        let engine = MockEngine {
            log,
            table: vec![("ぎ".to_string(), vec![])],
        };
        let mut c = Composer::new(engine);
        c.push_input("gi", "ぎ");
        assert!(c.begin_conversion());
        assert_eq!(c.display(), "gi");
    }

    #[test]
    fn 打鍵のみで未変換のローマ字もrawに含まれる() {
        let (mut c, _) = composer();
        c.push_input("gi", "ぎ");
        c.push_input("b", "");
        assert_eq!(c.raw(), "gib");
        c.pop_pending_raw();
        assert_eq!(c.raw(), "gi");
    }

    #[test]
    fn 入力すると変換が解除され候補が更新される() {
        let (mut c, log) = composer();
        type_kyou(&mut c);
        c.begin_conversion();
        c.advance();
        c.push_input("ha", "は");
        assert_eq!(c.phase(), Phase::Composing);
        assert_eq!(c.display(), "きょうは");
        assert_eq!(log.converts.borrow().last().unwrap().1, "きょうは");
    }

    #[test]
    fn 同一テキスト候補はdedupeされ先頭のengine_indexが残る() {
        let log = Log::default();
        let engine = MockEngine {
            log: log.clone(),
            table: vec![(
                "きょう".to_string(),
                vec!["今日".to_string(), "京".to_string(), "今日".to_string()],
            )],
        };
        let mut c = Composer::new(engine);
        type_kyou(&mut c);
        assert_eq!(c.candidates().len(), 2);
        assert_eq!(c.candidates()[0].text, "今日");
        assert_eq!(c.candidates()[0].engine_index, Some(0));
        assert_eq!(c.candidates()[1].text, "京");
        assert_eq!(c.candidates()[1].engine_index, Some(1));
    }

    #[test]
    fn backspaceで候補再計算_空になったら候補もクリア() {
        let (mut c, _) = composer();
        type_kyou(&mut c);
        assert!(c.backspace());
        assert_eq!(c.display(), "きょ");
        c.backspace();
        c.backspace();
        assert!(c.is_empty());
        assert_eq!(c.phase(), Phase::Empty);
        assert_eq!(c.display(), "");
        assert!(c.candidates().is_empty());
        assert_eq!(c.raw(), "");
        assert!(!c.backspace());
    }

    #[test]
    fn backspaceで単位ごと消えたかなの打鍵もrawから消える() {
        let (mut c, _) = composer();
        c.push_input("ka", "か");
        c.push_input("n", "ん");
        c.backspace();
        assert_eq!(c.raw(), "ka");
    }

    #[test]
    fn 変換中のbackspaceは未変換に戻る() {
        let (mut c, _) = composer();
        type_kyou(&mut c);
        c.begin_conversion();
        assert!(c.backspace());
        assert_eq!(c.phase(), Phase::Composing);
        assert_eq!(c.display(), "きょ");
    }

    #[test]
    fn revertで生ひらがな表示_次の入力で解除() {
        let (mut c, _) = composer();
        c.set_live(true);
        type_kyou(&mut c);
        c.begin_conversion();
        c.advance();
        c.revert();
        assert_eq!(c.phase(), Phase::Reverted);
        assert_eq!(c.display(), "きょう");
        c.push_input("ha", "は");
        assert_eq!(c.phase(), Phase::Composing);
        assert_eq!(c.display(), "きょうはK1");
    }

    #[test]
    fn 空のときrevertしても空のまま() {
        let (mut c, _) = composer();
        c.revert();
        assert_eq!(c.phase(), Phase::Empty);
    }

    #[test]
    fn 候補選択は範囲内のみ成功し_テキスト指定でも選べる() {
        let (mut c, _) = composer();
        type_kyou(&mut c);
        c.begin_conversion();
        assert!(!c.select_candidate(9));
        assert!(c.select_candidate(2));
        assert_eq!(c.display(), "京");
        assert!(c.select_candidate_text("教"));
        assert_eq!(c.display(), "教");
        assert!(!c.select_candidate_text("存在しない"));
    }

    #[test]
    fn 未変換のままcommitすると生ひらがなを返しcancelする() {
        let (mut c, log) = composer();
        type_kyou(&mut c);
        assert_eq!(c.commit(), "きょう");
        assert_eq!(*log.cancels.borrow(), 1);
        assert!(log.selects.borrow().is_empty());
        assert!(c.is_empty());
        assert_eq!(c.raw(), "");
    }

    #[test]
    fn 選択中の候補をcommitしその元indexで学習する() {
        let (mut c, log) = composer();
        type_kyou(&mut c);
        let gen = c.current_generation();
        c.begin_conversion();
        assert_eq!(c.commit(), "今日");
        assert_eq!(*log.selects.borrow(), vec![(gen, 0)]);

        type_kyou(&mut c);
        c.begin_conversion();
        c.select_candidate(2);
        assert_eq!(c.commit(), "京");
        assert_eq!(log.selects.borrow().last().unwrap().1, 1);
    }

    #[test]
    fn raw候補をcommitしても学習せずcancelする() {
        let (mut c, log) = composer();
        type_kyou(&mut c);
        c.begin_conversion();
        c.advance();
        assert_eq!(c.commit(), "kyou");
        assert!(log.selects.borrow().is_empty());
        assert_eq!(*log.cancels.borrow(), 1);
    }

    #[test]
    fn liveで変換前にcommitすると第1候補を学習して返す() {
        let (mut c, log) = composer();
        c.set_live(true);
        type_kyou(&mut c);
        assert_eq!(c.commit(), "今日");
        assert_eq!(log.selects.borrow().last().unwrap().1, 0);
    }

    #[test]
    fn revert後のcommitは生ひらがなでcancelする() {
        let (mut c, log) = composer();
        c.set_live(true);
        type_kyou(&mut c);
        c.revert();
        assert_eq!(c.commit(), "きょう");
        assert_eq!(*log.cancels.borrow(), 1);
    }

    #[test]
    fn 英単語は変換せずそのまま表示しcommitする() {
        let (mut c, log) = composer();
        c.push_literal("G");
        c.push_literal("it");
        assert_eq!(c.phase(), Phase::Literal);
        assert_eq!(c.display(), "Git");
        assert!(log.converts.borrow().is_empty());
        assert!(!c.begin_conversion());
        assert_eq!(c.commit(), "Git");
        assert_eq!(*log.cancels.borrow(), 1);
        assert_eq!(c.phase(), Phase::Empty);
    }

    #[test]
    fn 英単語のbackspaceは1文字ずつ消え空で終わる() {
        let (mut c, _) = composer();
        c.push_literal("Hi");
        assert!(c.backspace());
        assert_eq!(c.display(), "H");
        assert_eq!(c.phase(), Phase::Literal);
        assert_eq!(c.raw(), "H");
        assert!(c.backspace());
        assert_eq!(c.phase(), Phase::Empty);
    }

    #[test]
    fn 非liveのリランキングは保留され最初の変換で反映される() {
        let (mut c, log) = composer();
        type_kyou(&mut c);
        let gen = c.current_generation();
        assert_eq!(c.apply_rerank(gen, 1), None);
        assert_eq!(c.display(), "きょう");
        assert_eq!(texts(&c), vec!["今日", "京", "教"]);
        c.begin_conversion();
        assert_eq!(c.display(), "京");
        assert_eq!(texts(&c), vec!["京", "kyou", "今日", "教"]);
        assert_eq!(c.commit(), "京");
        assert_eq!(log.selects.borrow().last().unwrap().1, 1);
    }

    #[test]
    fn 保留中のリランキングは後の結果で上書きされる() {
        let (mut c, _) = composer();
        type_kyou(&mut c);
        let gen = c.current_generation();
        c.apply_rerank(gen, 1);
        c.apply_rerank(gen, 2);
        c.begin_conversion();
        assert_eq!(c.display(), "教");
    }

    #[test]
    fn 保留中のリランキングは入力が進むと破棄される() {
        let (mut c, _) = composer();
        type_kyou(&mut c);
        let gen = c.current_generation();
        c.apply_rerank(gen, 1);
        c.push_input("ha", "は");
        c.begin_conversion();
        assert_eq!(c.display(), "きょうはK1");
    }

    #[test]
    fn 変換を始めた後のリランキングは並べ替えない() {
        let (mut c, _) = composer();
        type_kyou(&mut c);
        let gen = c.current_generation();
        c.begin_conversion();
        assert_eq!(c.apply_rerank(gen, 1), None);
        c.begin_conversion();
        assert_eq!(texts(&c), vec!["今日", "kyou", "京", "教"]);
        c.advance();
        assert_eq!(c.apply_rerank(gen, 2), None);
        assert_eq!(texts(&c), vec!["今日", "kyou", "京", "教"]);
    }

    #[test]
    fn liveのリランキングは即座に表示順を変えengine_indexで学習する() {
        let (mut c, log) = composer();
        c.set_live(true);
        type_kyou(&mut c);
        let gen = c.current_generation();
        assert_eq!(c.apply_rerank(gen, 1), Some("京".to_string()));
        assert_eq!(c.display(), "京");
        assert_eq!(c.candidates()[0].engine_index, Some(1));
        assert_eq!(c.candidates()[1].text, "今日");
        assert_eq!(c.commit(), "京");
        assert_eq!(log.selects.borrow().last().unwrap().1, 1);
    }

    #[test]
    fn リランキングは旧世代_範囲外_先頭指定で無効() {
        let (mut c, _) = composer();
        type_kyou(&mut c);
        let old = c.current_generation();
        c.push_input("ha", "は");
        assert_eq!(c.apply_rerank(old, 1), None);

        let (mut c2, _) = composer();
        type_kyou(&mut c2);
        let gen2 = c2.current_generation();
        assert_eq!(c2.apply_rerank(gen2, 99), None);
        assert_eq!(c2.apply_rerank(gen2, 0), None);
        c2.begin_conversion();
        assert_eq!(c2.display(), "今日");
    }

    #[test]
    fn revert後のリランキングは無効() {
        let (mut c, _) = composer();
        c.set_live(true);
        type_kyou(&mut c);
        let gen = c.current_generation();
        c.revert();
        assert_eq!(c.apply_rerank(gen, 1), None);
    }
}
