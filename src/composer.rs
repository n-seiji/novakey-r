// composer.rs - 未確定文の状態機械: バッファ・世代管理・差分抑制
// LLM は KanjiConverter trait として注入する(テストではモック)。

/// LLM かな漢字変換への非同期リクエスト発行口。
/// 結果は Composer::on_llm_result に (generation, text) で戻す契約。
pub trait KanjiConverter {
    fn request(&mut self, generation: u64, hiragana: &str, context: &str);
}

pub struct Composer<C: KanjiConverter> {
    converter: C,
    /// 未確定のひらがなバッファ(常に入力の真実源)
    hiragana: String,
    /// 現世代の LLM 変換結果(あれば display はこちら)
    converted: Option<String>,
    /// 変換要求の世代。入力変更・確定・キャンセルで進む
    generation: u64,
}

impl<C: KanjiConverter> Composer<C> {
    pub fn new(converter: C) -> Self {
        Self {
            converter,
            hiragana: String::new(),
            converted: None,
            generation: 0,
        }
    }

    pub fn current_generation(&self) -> u64 {
        self.generation
    }

    pub fn is_empty(&self) -> bool {
        self.hiragana.is_empty()
    }

    pub fn display(&self) -> &str {
        self.converted.as_deref().unwrap_or(&self.hiragana)
    }

    fn invalidate(&mut self) {
        self.generation += 1;
        self.converted = None;
    }

    pub fn push_hiragana(&mut self, kana: &str) {
        // 変換結果が表示中なら、それを土台に追記する(スペース変換後の追加入力仕様)
        if let Some(converted) = self.converted.take() {
            self.hiragana = converted;
        }
        self.hiragana.push_str(kana);
        self.invalidate();
    }

    pub fn backspace(&mut self) -> bool {
        if let Some(converted) = self.converted.take() {
            self.hiragana = converted;
        }
        if self.hiragana.pop().is_none() {
            return false;
        }
        self.invalidate();
        true
    }

    pub fn on_llm_result(&mut self, generation: u64, text: &str) -> Option<String> {
        if generation != self.generation {
            return None; // 旧世代: 破棄
        }
        if self.converted.as_deref() == Some(text) {
            return None; // 差分なし: setMarkedText を呼ばせない
        }
        self.converted = Some(text.to_string());
        Some(text.to_string())
    }

    pub fn commit(&mut self) -> String {
        let out = self.display().to_string();
        self.hiragana.clear();
        self.invalidate();
        out
    }

    pub fn cancel(&mut self) {
        self.converted = None;
        self.generation += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Default)]
    struct MockLlm {
        requests: Rc<RefCell<Vec<(u64, String, String)>>>,
    }
    impl KanjiConverter for MockLlm {
        fn request(&mut self, generation: u64, hiragana: &str, context: &str) {
            self.requests
                .borrow_mut()
                .push((generation, hiragana.to_string(), context.to_string()));
        }
    }

    fn composer_with_log() -> (Composer<MockLlm>, Rc<RefCell<Vec<(u64, String, String)>>>) {
        let log = Rc::new(RefCell::new(Vec::new()));
        (
            Composer::new(MockLlm {
                requests: log.clone(),
            }),
            log,
        )
    }

    #[test]
    fn 入力なしでは空() {
        let (c, _) = composer_with_log();
        assert!(c.is_empty());
        assert_eq!(c.display(), "");
    }

    #[test]
    fn ひらがな追記でdisplayは生ひらがな() {
        let (mut c, _) = composer_with_log();
        c.push_hiragana("きょう");
        assert_eq!(c.display(), "きょう");
    }

    #[test]
    fn 現世代のllm結果はmarked_textとして返る() {
        let (mut c, _) = composer_with_log();
        c.push_hiragana("きょうはいいてんき");
        let gen = c.current_generation();
        assert_eq!(
            c.on_llm_result(gen, "今日はいい天気"),
            Some("今日はいい天気".to_string())
        );
        assert_eq!(c.display(), "今日はいい天気");
    }

    #[test]
    fn 旧世代のllm結果は破棄される() {
        let (mut c, _) = composer_with_log();
        c.push_hiragana("きょう");
        let old_gen = c.current_generation();
        c.push_hiragana("は"); // 入力で世代が進む
        assert_eq!(c.on_llm_result(old_gen, "今日"), None);
        assert_eq!(c.display(), "きょうは");
    }

    #[test]
    fn 同一表示の結果は差分抑制でnone() {
        let (mut c, _) = composer_with_log();
        c.push_hiragana("きょう");
        let gen = c.current_generation();
        assert_eq!(c.on_llm_result(gen, "今日"), Some("今日".to_string()));
        // 同じ世代・同じ文字列の再着信(理論上)や再設定は None
        assert_eq!(c.on_llm_result(gen, "今日"), None);
    }

    #[test]
    fn commitで全状態クリアとin_flight無効化() {
        let (mut c, _) = composer_with_log();
        c.push_hiragana("きょう");
        let gen = c.current_generation();
        assert_eq!(c.commit(), "きょう");
        assert!(c.is_empty());
        // commit 後に旧世代の結果が届いても無視される
        assert_eq!(c.on_llm_result(gen, "今日"), None);
    }

    #[test]
    fn 変換済みをcommitすると変換結果が返る() {
        let (mut c, _) = composer_with_log();
        c.push_hiragana("きょう");
        let gen = c.current_generation();
        c.on_llm_result(gen, "今日");
        assert_eq!(c.commit(), "今日");
    }

    #[test]
    fn cancelで生ひらがなに戻る() {
        let (mut c, _) = composer_with_log();
        c.push_hiragana("きょう");
        let gen = c.current_generation();
        c.on_llm_result(gen, "今日");
        c.cancel();
        assert_eq!(c.display(), "きょう");
        // cancel も in-flight を無効化
        assert_eq!(c.on_llm_result(gen, "今日"), None);
    }

    #[test]
    fn backspaceはひらがなバッファ末尾を削る() {
        let (mut c, _) = composer_with_log();
        c.push_hiragana("きょう");
        assert!(c.backspace());
        assert_eq!(c.display(), "きょ");
        c.backspace();
        c.backspace();
        assert!(!c.backspace()); // 空なら false
    }
}
