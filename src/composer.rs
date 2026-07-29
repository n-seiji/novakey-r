// composer.rs - 未確定文の状態機械: ひらがなバッファ + 同期変換候補 + 選択状態。
// 候補は {text, engine_index} で持ち、リランキング後も学習には元 index を渡す。

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub text: String,
    pub engine_index: usize,
}

pub trait CandidateEngine {
    fn convert(&mut self, revision: u64, hiragana: &str) -> Vec<String>;
    fn select(&mut self, revision: u64, engine_index: usize) -> bool;
    fn cancel(&mut self);
}

pub struct Composer<E: CandidateEngine> {
    engine: E,
    hiragana: String,
    candidates: Vec<Candidate>,
    selected: Option<usize>,
    raw: bool,
    live: bool,
    generation: u64,
}

impl<E: CandidateEngine> Composer<E> {
    pub fn new(engine: E) -> Self {
        Self {
            engine,
            hiragana: String::new(),
            candidates: Vec::new(),
            selected: None,
            raw: false,
            live: true,
            generation: 0,
        }
    }

    pub fn set_live(&mut self, live: bool) {
        self.live = live;
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

    pub fn selected_index(&self) -> Option<usize> {
        self.selected
    }

    pub fn display_hiragana(&self) -> &str {
        &self.hiragana
    }

    pub fn is_raw(&self) -> bool {
        self.raw
    }

    pub fn display(&self) -> &str {
        if self.raw {
            return &self.hiragana;
        }
        if let Some(i) = self.selected {
            return &self.candidates[i].text;
        }
        if self.live {
            if let Some(first) = self.candidates.first() {
                return &first.text;
            }
        }
        &self.hiragana
    }

    fn refresh(&mut self) {
        self.generation += 1;
        self.selected = None;
        self.raw = false;
        self.candidates.clear();
        if self.hiragana.is_empty() {
            return;
        }
        let texts = self.engine.convert(self.generation, &self.hiragana);
        for (engine_index, text) in texts.into_iter().enumerate() {
            if !self.candidates.iter().any(|c| c.text == text) {
                self.candidates.push(Candidate { text, engine_index });
            }
        }
    }

    pub fn push_hiragana(&mut self, kana: &str) {
        self.hiragana.push_str(kana);
        self.refresh();
    }

    pub fn backspace(&mut self) -> bool {
        if self.hiragana.pop().is_none() {
            return false;
        }
        self.refresh();
        true
    }

    pub fn cycle(&mut self) -> bool {
        if self.candidates.is_empty() {
            return false;
        }
        self.raw = false;
        self.selected = Some(match self.selected {
            None => 0,
            Some(i) => (i + 1) % self.candidates.len(),
        });
        true
    }

    pub fn revert_raw(&mut self) {
        self.selected = None;
        self.raw = true;
    }

    pub fn commit(&mut self) -> String {
        let out = self.display().to_string();
        let committed_display_index = if self.raw {
            None
        } else {
            self.selected.or(if self.live && !self.candidates.is_empty() {
                Some(0)
            } else {
                None
            })
        };
        match committed_display_index {
            Some(i) => {
                let _ = self
                    .engine
                    .select(self.generation, self.candidates[i].engine_index);
            }
            None => self.engine.cancel(),
        }
        self.hiragana.clear();
        self.candidates.clear();
        self.selected = None;
        self.raw = false;
        self.generation += 1;
        out
    }

    pub fn apply_rerank(&mut self, generation: u64, display_index: usize) -> Option<String> {
        if generation != self.generation
            || self.selected.is_some()
            || self.raw
            || display_index == 0
            || display_index >= self.candidates.len()
        {
            return None;
        }
        let chosen = self.candidates.remove(display_index);
        self.candidates.insert(0, chosen);
        Some(self.display().to_string())
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

    #[test]
    fn 入力なしでは空() {
        let (c, _) = composer();
        assert!(c.is_empty());
        assert_eq!(c.display(), "");
    }

    #[test]
    fn liveなら第1候補を表示() {
        let (mut c, log) = composer();
        c.push_hiragana("きょう");
        assert_eq!(c.display(), "今日");
        assert_eq!(log.converts.borrow().len(), 1);
    }

    #[test]
    fn live_offなら生ひらがな表示() {
        let (mut c, _) = composer();
        c.set_live(false);
        c.push_hiragana("きょう");
        assert_eq!(c.display(), "きょう");
    }

    #[test]
    fn cycleで候補を順送りしwrapする() {
        let (mut c, _) = composer();
        c.push_hiragana("きょう");
        assert!(c.cycle());
        assert_eq!(c.display(), "今日");
        c.cycle();
        assert_eq!(c.display(), "京");
        c.cycle();
        c.cycle();
        assert_eq!(c.display(), "今日");
        assert_eq!(c.selected_index(), Some(0));
    }

    #[test]
    fn 候補なしならcycleはfalse() {
        let (mut c, _) = composer();
        assert!(!c.cycle());
    }

    #[test]
    fn 入力すると選択が解除され候補が更新される() {
        let (mut c, log) = composer();
        c.push_hiragana("きょう");
        c.cycle();
        c.cycle();
        c.push_hiragana("は");
        assert_eq!(c.selected_index(), None);
        assert_eq!(c.display(), "きょうはK1");
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
        c.push_hiragana("きょう");
        assert_eq!(c.candidates().len(), 2);
        assert_eq!(c.candidates()[0].text, "今日");
        assert_eq!(c.candidates()[0].engine_index, 0);
        assert_eq!(c.candidates()[1].text, "京");
        assert_eq!(c.candidates()[1].engine_index, 1);
    }

    #[test]
    fn backspaceで候補再計算_空になったら候補もクリア() {
        let (mut c, _) = composer();
        c.push_hiragana("きょう");
        assert!(c.backspace());
        assert_eq!(c.display(), "きょK1");
        c.backspace();
        c.backspace();
        assert!(c.is_empty());
        assert_eq!(c.display(), "");
        assert!(c.candidates().is_empty());
        assert!(!c.backspace());
    }

    #[test]
    fn revert_rawで生ひらがな表示_次の入力で解除() {
        let (mut c, _) = composer();
        c.push_hiragana("きょう");
        c.cycle();
        c.revert_raw();
        assert_eq!(c.display(), "きょう");
        assert_eq!(c.selected_index(), None);
        c.push_hiragana("は");
        assert_eq!(c.display(), "きょうはK1");
    }

    #[test]
    fn commitは候補表示中ならselect_生ひらがなならcancel() {
        let (mut c, log) = composer();
        let indices = |log: &Log| -> Vec<usize> {
            log.selects.borrow().iter().map(|s| s.1).collect()
        };
        c.push_hiragana("きょう");
        let gen = c.current_generation();
        assert_eq!(c.commit(), "今日");
        assert_eq!(*log.selects.borrow(), vec![(gen, 0)]);
        assert!(c.is_empty());

        c.push_hiragana("きょう");
        c.cycle();
        c.cycle();
        assert_eq!(c.commit(), "京");
        assert_eq!(indices(&log), vec![0, 1]);

        c.push_hiragana("きょう");
        c.revert_raw();
        assert!(c.is_raw());
        assert_eq!(c.commit(), "きょう");
        assert_eq!(*log.cancels.borrow(), 1);
    }

    #[test]
    fn apply_rerankは表示順のみ変えengine_indexで学習する() {
        let (mut c, log) = composer();
        c.push_hiragana("きょう");
        let gen = c.current_generation();
        assert_eq!(c.apply_rerank(gen, 1), Some("京".to_string()));
        assert_eq!(c.display(), "京");
        assert_eq!(c.candidates()[0].text, "京");
        assert_eq!(c.candidates()[0].engine_index, 1);
        assert_eq!(c.candidates()[1].text, "今日");
        assert_eq!(c.commit(), "京");
        assert_eq!(log.selects.borrow().last().unwrap().1, 1);
    }

    #[test]
    fn apply_rerankは旧世代_ユーザー操作後_範囲外で無効() {
        let (mut c, _) = composer();
        c.push_hiragana("きょう");
        let old = c.current_generation();
        c.push_hiragana("は");
        assert_eq!(c.apply_rerank(old, 1), None);

        let gen = c.current_generation();
        c.cycle();
        assert_eq!(c.apply_rerank(gen, 1), None);

        let (mut c2, _) = composer();
        c2.push_hiragana("きょう");
        let gen2 = c2.current_generation();
        assert_eq!(c2.apply_rerank(gen2, 99), None);
        assert_eq!(c2.apply_rerank(gen2, 0), None);
    }
}
