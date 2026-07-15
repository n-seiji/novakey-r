// composer.rs - 未確定文の状態機械: バッファ・世代管理・差分抑制
// LLM は KanjiConverter trait として注入する(テストではモック)。

/// LLM かな漢字変換への非同期リクエスト発行口。
/// 結果は Composer::on_llm_result に (generation, text) で戻す契約。
pub trait KanjiConverter {
    fn request(&mut self, generation: u64, hiragana: &str, context: &str);
}

/// 入力停止からこの時間経過で変換要求を出す
pub const DEBOUNCE_MS: u64 = 300;
/// 未確定がこれを超えたら tick での自動変換を止める(コンテキスト保護)
pub const MAX_PENDING_CHARS: usize = 500;
/// プロンプトに含める確定済み文脈の上限(4096トークン制約対策)
pub const MAX_CONTEXT_CHARS: usize = 200;

pub struct Composer<C: KanjiConverter> {
    converter: C,
    /// 未確定のひらがなバッファ(常に入力の真実源)
    hiragana: String,
    /// 現世代の LLM 変換結果(あれば display はこちら)
    converted: Option<String>,
    /// 変換要求の世代。入力変更・確定・キャンセルで進む
    generation: u64,
    /// 最後に入力があった時刻(ms)。tick のデバウンス判定に使う
    last_input_ms: Option<u64>,
    /// この世代で既に要求を出したか(二重要求防止)
    requested_generation: Option<u64>,
    /// 確定済みの直近文脈(MAX_CONTEXT_CHARS に切り詰め済み)
    context: String,
}

impl<C: KanjiConverter> Composer<C> {
    pub fn new(converter: C) -> Self {
        Self {
            converter,
            hiragana: String::new(),
            converted: None,
            generation: 0,
            last_input_ms: None,
            requested_generation: None,
            context: String::new(),
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

    pub fn note_input_time(&mut self, now_ms: u64) {
        self.last_input_ms = Some(now_ms);
    }

    pub fn set_context(&mut self, ctx: &str) {
        // 直近 MAX_CONTEXT_CHARS 文字のみ保持(4096トークン制約対策)
        let chars: Vec<char> = ctx.chars().collect();
        let start = chars.len().saturating_sub(MAX_CONTEXT_CHARS);
        self.context = chars[start..].iter().collect();
    }

    fn should_request(&self) -> bool {
        !self.hiragana.is_empty()
            && self.converted.is_none()
            && self.requested_generation != Some(self.generation)
            && self.hiragana.chars().count() <= MAX_PENDING_CHARS
    }

    pub fn tick(&mut self, now_ms: u64) {
        let Some(last) = self.last_input_ms else {
            return;
        };
        if now_ms.saturating_sub(last) < DEBOUNCE_MS {
            return;
        }
        if self.should_request() {
            self.requested_generation = Some(self.generation);
            self.converter
                .request(self.generation, &self.hiragana, &self.context);
        }
    }

    /// スペース変換モード用: 即時に変換要求。
    /// 明示操作のため MAX_PENDING_CHARS 超過でも発行する。
    pub fn request_now(&mut self) {
        if !self.hiragana.is_empty() && self.requested_generation != Some(self.generation) {
            self.requested_generation = Some(self.generation);
            self.converter
                .request(self.generation, &self.hiragana, &self.context);
        }
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

    #[test]
    fn 入力後300ms経過のtickで変換要求が飛ぶ() {
        let (mut c, log) = composer_with_log();
        c.push_hiragana("きょう");
        c.note_input_time(1000);
        c.tick(1200); // まだ 200ms
        assert!(log.borrow().is_empty());
        c.tick(1300); // 300ms 経過
        let reqs = log.borrow();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].1, "きょう");
        assert_eq!(reqs[0].0, c.current_generation());
    }

    #[test]
    fn 同一世代で二重要求しない() {
        let (mut c, log) = composer_with_log();
        c.push_hiragana("きょう");
        c.note_input_time(1000);
        c.tick(1300);
        c.tick(1400);
        assert_eq!(log.borrow().len(), 1);
    }

    #[test]
    fn 変換済みなら再要求しない() {
        let (mut c, log) = composer_with_log();
        c.push_hiragana("きょう");
        c.note_input_time(1000);
        c.tick(1300);
        let gen = c.current_generation();
        c.on_llm_result(gen, "今日");
        c.tick(2000);
        assert_eq!(log.borrow().len(), 1);
    }

    #[test]
    fn 文脈は200文字に切り詰められる() {
        let (mut c, log) = composer_with_log();
        c.set_context(&"あ".repeat(300));
        c.push_hiragana("きょう");
        c.request_now();
        assert_eq!(log.borrow()[0].2.chars().count(), 200);
    }

    #[test]
    fn 未確定500文字超はtickで要求しない() {
        let (mut c, log) = composer_with_log();
        c.push_hiragana(&"か".repeat(501));
        c.note_input_time(1000);
        c.tick(2000);
        assert!(log.borrow().is_empty());
    }

    #[test]
    fn request_nowは即時要求() {
        let (mut c, log) = composer_with_log();
        c.push_hiragana("きょう");
        c.request_now();
        assert_eq!(log.borrow().len(), 1);
    }

    #[test]
    fn 空バッファでは要求しない() {
        let (mut c, log) = composer_with_log();
        c.request_now();
        c.note_input_time(0);
        c.tick(1000);
        assert!(log.borrow().is_empty());
    }
}
