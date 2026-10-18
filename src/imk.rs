use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use objc2::rc::{Allocated, Id};
use objc2::runtime::{AnyObject, Bool};
use objc2::{
    declare_class, extern_class, msg_send_id, mutability, sel, ClassType, DeclaredClass,
};
use objc2_foundation::{NSArray, NSObject, NSString};
use once_cell::sync::Lazy;
use rand::Rng;

use crate::composer::{CandidateEngine, Composer, Phase};
use crate::imk_client::Client;
use crate::keymap::{self, Action, Key, Stroke};
use crate::kkc_bridge;
use crate::llm_bridge;
use crate::outbound::{self, Outbound, Snapshot};
use crate::platform;
use crate::romaji_converter::{Flush, RomajiConverter};
use crate::symbols;
use crate::trace;

pub struct KkcEngine {
    controller_id: u64,
}

impl CandidateEngine for KkcEngine {
    fn convert(&mut self, revision: u64, hiragana: &str) -> Vec<String> {
        kkc_bridge::convert(self.controller_id, revision, hiragana)
    }
    fn select(&mut self, revision: u64, engine_index: usize) -> bool {
        kkc_bridge::select(self.controller_id, revision, engine_index)
    }
    fn cancel(&mut self) {
        kkc_bridge::cancel(self.controller_id);
    }
}

use std::sync::atomic::{AtomicU64, Ordering};
static NEXT_CONTROLLER_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static CONTROLLERS: RefCell<HashMap<u64, Id<NovakeyRInputController>>> =
        RefCell::new(HashMap::new());
    static CREDENTIALS_MISSING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static AI_RERANK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

const AI_RERANK_KEY: &str = "NovakeyRAIRerankEnabled";

fn dispatch_llm_result(controller_id: u64, generation: u64, status: i32, text: &str) {
    CONTROLLERS.with(|c| {
        let controllers = c.borrow();
        let Some(controller) = controllers.get(&controller_id) else {
            return;
        };
        controller.handle_llm_result(generation, status, text);
    });
}

pub fn init_llm_dispatch() {
    AI_RERANK.with(|e| e.set(platform::defaults_bool(AI_RERANK_KEY)));
    llm_bridge::set_result_handler(dispatch_llm_result);
}

fn ai_rerank_enabled() -> bool {
    AI_RERANK.with(|e| e.get())
}

fn credentials_missing() -> bool {
    CREDENTIALS_MISSING.with(|e| e.get())
}

fn set_credentials_missing(missing: bool) {
    CREDENTIALS_MISSING.with(|e| e.set(missing));
}

/// 無効時は Keychain に触れる経路(llm_bridge)へ一切進まないための門番。
fn rerank_allowed(ai_enabled: bool, credentials_missing: bool) -> bool {
    ai_enabled && !credentials_missing
}

extern_class!(
    #[derive(Debug, PartialEq, Eq, Hash)]
    pub struct IMKInputController;

    unsafe impl ClassType for IMKInputController {
        type Super = NSObject;
        type Mutability = mutability::InteriorMutable;
        const NAME: &'static str = "IMKInputController";
    }
);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ConversionMode {
    Live,
    Space,
}

pub struct Ivars {
    converter: RefCell<RomajiConverter>,
    composer: RefCell<Composer<KkcEngine>>,
    mode: RefCell<ConversionMode>,
    controller_id: u64,
    last_client: RefCell<Option<Client>>,
    /// show_selection がパネルを更新している間 true。パネルが同期で呼び戻す candidateSelectionChanged: を無視する。
    syncing: Cell<bool>,
    /// 候補パネルがハイライトしている行。パネルの行番号 API は使えないので、こちらで追跡する。
    panel_line: Cell<usize>,
}

declare_class!(
    pub struct NovakeyRInputController;

    unsafe impl ClassType for NovakeyRInputController {
        type Super = IMKInputController;
        type Mutability = mutability::InteriorMutable;
        const NAME: &'static str = "NovakeyRInputController";
    }

    impl DeclaredClass for NovakeyRInputController {
        type Ivars = Ivars;
    }

    unsafe impl NovakeyRInputController {
        #[method_id(initWithServer:delegate:client:)]
        fn init_with_server(
            this: Allocated<Self>,
            server: *mut AnyObject,
            delegate: *mut AnyObject,
            client: *mut AnyObject,
        ) -> Option<Id<Self>> {
            let controller_id = NEXT_CONTROLLER_ID.fetch_add(1, Ordering::SeqCst);
            let this = this.set_ivars(Ivars {
                converter: RefCell::new(RomajiConverter::new()),
                composer: RefCell::new(Composer::new(KkcEngine { controller_id })),
                mode: RefCell::new(ConversionMode::Space),
                controller_id,
                last_client: RefCell::new(None),
                syncing: Cell::new(false),
                panel_line: Cell::new(0),
            });
            // SAFETY: 親クラスの指定イニシャライザに、IMK から受け取った引数をそのまま渡す。
            let this: Option<Id<Self>> = unsafe {
                msg_send_id![super(this), initWithServer: server, delegate: delegate, client: client]
            };
            if let Some(ref obj) = this {
                CONTROLLERS.with(|c| c.borrow_mut().insert(controller_id, obj.clone()));
            }
            this
        }

        #[method(recognizedEvents:)]
        fn recognized_events(&self, _client: Option<&AnyObject>) -> usize {
            platform::NS_EVENT_MASK_KEY_DOWN
        }

        /// キー入力の唯一の入口。候補パネル表示中も先にここへ届く (make_panel 参照)。
        #[method(handleEvent:client:)]
        fn handle_event(&self, event: Option<&AnyObject>, client: Option<&AnyObject>) -> Bool {
            let (Some(event), Some(client)) = (event.and_then(platform::key_event), client) else {
                return Bool::NO;
            };
            if event.is_shortcut {
                trace::record("handleEvent", "key=Shortcut action=PassThrough");
                return Bool::NO;
            }
            let client = Client::new(client);
            *self.ivars().last_client.borrow_mut() = Some(client.clone());
            let handled = match keymap::stroke(event.key_code, &event.characters, event.is_shift) {
                Stroke::Keys(keys) => keys.iter().fold(false, |handled, (key, text)| {
                    self.dispatch(*key, text, &client) || handled
                }),
                Stroke::Navigation => {
                    let has_pending = !self.ivars().converter.borrow().buffer().is_empty()
                        || !self.ivars().composer.borrow().is_empty();
                    trace::record(
                        "handleEvent",
                        &format!("key=Navigation phase={:?} handled={has_pending}", self.phase()),
                    );
                    has_pending
                }
            };
            Bool::new(handled)
        }

        #[method_id(candidates:)]
        fn candidates(&self, _sender: Option<&AnyObject>) -> Id<NSArray<NSString>> {
            let composer = self.ivars().composer.borrow();
            let texts: Vec<Id<NSString>> = composer
                .candidates()
                .iter()
                .map(|cand| NSString::from_str(&cand.text))
                .collect();
            NSArray::from_vec(texts)
        }

        #[method(candidateSelectionChanged:)]
        fn candidate_selection_changed(&self, candidate_string: Option<&AnyObject>) {
            if self.ivars().syncing.get() {
                return;
            }
            let Some(client) = self.client() else { return };
            let Some(text) = candidate_string.and_then(platform::candidate_text) else {
                return;
            };
            trace::record("candidateSelectionChanged", &format!("phase={:?}", self.phase()));
            let marked = {
                let mut composer = self.ivars().composer.borrow_mut();
                composer
                    .select_candidate_text(&text)
                    .then(|| composer.display().to_string())
            };
            if let Phase::Converting { selected, .. } = self.phase() {
                self.ivars().panel_line.set(selected);
            }
            if let Some(marked) = marked {
                client.set_marked(&marked);
            }
        }

        #[method(candidateSelected:)]
        fn candidate_selected(&self, candidate_string: Option<&AnyObject>) {
            let Some(client) = self.client() else { return };
            let Some(text) = candidate_string.and_then(platform::candidate_text) else {
                return;
            };
            trace::record("candidateSelected", &format!("phase={:?}", self.phase()));
            self.ivars().composer.borrow_mut().select_candidate_text(&text);
            self.commit_and_insert(&client, "");
        }

        #[method_id(menu)]
        fn menu(&self) -> Id<AnyObject> {
            let mode_title = match *self.ivars().mode.borrow() {
                ConversionMode::Live => "ライブ変換: オン",
                ConversionMode::Space => "ライブ変換: オフ(スペースで変換)",
            };
            let ai_title = if ai_rerank_enabled() {
                "AI並べ替え: オン"
            } else {
                "AI並べ替え: オフ"
            };
            let warning = (ai_rerank_enabled() && credentials_missing())
                .then_some("LLMリランキング無効(Cloudflare認証情報を確認)");
            platform::build_menu(
                &[
                    (mode_title, sel!(toggleConversionMode:)),
                    (ai_title, sel!(toggleAIRerank:)),
                ],
                warning,
            )
        }

        #[method(toggleAIRerank:)]
        fn toggle_ai_rerank(&self, _sender: *mut AnyObject) {
            let enabled = !ai_rerank_enabled();
            AI_RERANK.with(|e| e.set(enabled));
            platform::set_defaults_bool(AI_RERANK_KEY, enabled);
            if enabled {
                let avail = llm_bridge::availability();
                set_credentials_missing(avail != llm_bridge::Availability::Available);
                if credentials_missing() {
                    platform::log(&format!("LLM unavailable: {:?}", avail));
                }
            }
        }

        #[method(toggleConversionMode:)]
        fn toggle_conversion_mode(&self, _sender: *mut AnyObject) {
            let new_mode = {
                let mut mode = self.ivars().mode.borrow_mut();
                *mode = match *mode {
                    ConversionMode::Live => ConversionMode::Space,
                    ConversionMode::Space => ConversionMode::Live,
                };
                *mode
            };
            self.ivars().composer.borrow_mut().set_live(new_mode == ConversionMode::Live);
            let is_empty = self.ivars().composer.borrow().is_empty();
            if let (Some(client), false) = (self.client(), is_empty) {
                self.refresh_marked(&client);
            }
        }

        #[method(commitComposition:)]
        fn commit_composition(&self, client: Option<&AnyObject>) {
            let client = client.map(Client::new);
            self.flush_romaji();
            let is_empty = self.ivars().composer.borrow().is_empty();
            if let (Some(client), false) = (client, is_empty) {
                let text = self.ivars().composer.borrow_mut().commit();
                client.insert(&text);
            }
            self.hide_candidates_window();
        }
    }
);

/// リランキング要求の中身。composer の借用中に作り、借用を外してから送る。
struct RerankJob {
    generation: u64,
    hiragana: String,
    candidates: Vec<String>,
}

impl NovakeyRInputController {
    fn client(&self) -> Option<Client> {
        self.ivars().last_client.borrow().clone()
    }

    fn phase(&self) -> Phase {
        self.ivars().composer.borrow().phase()
    }

    /// 表示中の変換結果(またはかな) + 未変換のローマ字。
    fn marked_text(&self) -> String {
        let romaji = self.ivars().converter.borrow().pending_display();
        let display = self.ivars().composer.borrow().display().to_string();
        format!("{display}{romaji}")
    }

    fn refresh_marked(&self, client: &Client) {
        client.set_marked(&self.marked_text());
    }

    /// 未変換のローマ字を確定かなとして composer へ流す。
    fn flush_romaji(&self) {
        let flush = self.ivars().converter.borrow_mut().flush();
        let ranks = matches!(flush, Flush::Kana(_));
        self.ivars().composer.borrow_mut().push_flush(flush);
        if ranks {
            self.request_rerank(false);
        }
    }

    fn push_input(&self, raw: &str, kana: &str) {
        let carry = self.ivars().converter.borrow().buffer().to_string();
        self.ivars()
            .composer
            .borrow_mut()
            .push_input(raw, kana, &carry);
        if !kana.is_empty() {
            self.request_rerank(false);
        }
    }

    /// 現在の表示内容を確定して挿入する。suffix は確定直後に続けて入れる文字列。
    fn commit_and_insert(&self, client: &Client, suffix: &str) {
        let text = self.ivars().composer.borrow_mut().commit();
        client.set_marked("");
        client.insert(&format!("{text}{suffix}"));
        self.hide_candidates_window();
    }

    /// キー処理の入口。キーを動作表で解決して実行する。処理したら true。
    fn dispatch(&self, key: Key, text: &str, client: &Client) -> bool {
        let phase = self.phase();
        if let Some(handled) = self.prepare(key, client) {
            trace::record(
                "handleEvent",
                &format!("key={} phase={phase:?} action=Romaji handled={handled}", key.name()),
            );
            return handled;
        }
        let phase = self.phase();
        let action = keymap::action_for(phase, key);
        let handled = self.perform(action, text, client);
        trace::record(
            "handleEvent",
            &format!(
                "key={} phase={phase:?} action={} handled={handled} now={:?}",
                key.name(),
                action.name(),
                self.phase()
            ),
        );
        handled
    }

    /// 動作表に引く前の、ローマ字バッファ側の後始末。ここで処理が済んだら Some。
    fn prepare(&self, key: Key, client: &Client) -> Option<bool> {
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
        );
        if flushes {
            self.flush_romaji();
        }
        let has_romaji = !self.ivars().converter.borrow().buffer().is_empty();
        match key {
            Key::Backspace if has_romaji => {
                self.ivars().converter.borrow_mut().handle_backspace();
                self.ivars().composer.borrow_mut().pop_pending_raw();
                self.refresh_marked(client);
                Some(true)
            }
            Key::Escape => {
                self.ivars().converter.borrow_mut().clear();
                self.ivars().composer.borrow_mut().clear_pending_raw();
                if has_romaji && self.ivars().composer.borrow().is_empty() {
                    client.set_marked("");
                    return Some(true);
                }
                None
            }
            _ => None,
        }
    }

    fn perform(&self, action: Action, text: &str, client: &Client) -> bool {
        match action {
            Action::PassThrough => false,
            Action::InsertSpace => {
                client.insert(" ");
                true
            }
            Action::InsertDirect => playful_convert(text)
                .map(|converted| client.insert(&converted))
                .is_some(),
            Action::TypeRomaji => {
                let kana = text
                    .chars()
                    .filter_map(|ch| {
                        self.ivars()
                            .converter
                            .borrow_mut()
                            .process_input(ch.to_ascii_lowercase())
                    })
                    .collect::<String>();
                self.push_input(text, &kana);
                self.show_typing(client)
            }
            Action::StartLiteral | Action::TypeLiteral => {
                self.ivars().composer.borrow_mut().push_literal(text);
                self.show_typing(client)
            }
            Action::TypeText => {
                self.push_input(text, symbols::default_form(text).unwrap_or(text));
                self.show_typing(client)
            }
            Action::Convert => {
                let needs_rerank = {
                    let composer = self.ivars().composer.borrow();
                    composer.phase() != Phase::Dangling && !composer.has_pending_rerank()
                };
                if needs_rerank {
                    self.request_rerank(true);
                }
                self.ivars().composer.borrow_mut().begin_conversion();
                self.refresh_marked(client);
                true
            }
            Action::NextCandidate => {
                self.ivars().composer.borrow_mut().advance();
                self.show_selection(client)
            }
            Action::PrevCandidate => {
                self.ivars().composer.borrow_mut().retreat();
                self.show_selection(client)
            }
            Action::CommitThenPassThrough => {
                self.commit_and_insert(client, "");
                false
            }
            Action::PickCandidate(number) => {
                let picked = self
                    .ivars()
                    .composer
                    .borrow_mut()
                    .select_candidate(usize::from(number) - 1);
                if picked {
                    self.commit_and_insert(client, "");
                }
                true
            }
            Action::Commit => {
                self.commit_and_insert(client, "");
                true
            }
            Action::CommitThenSpace => {
                self.commit_and_insert(client, " ");
                true
            }
            Action::CommitThenInsert => {
                self.commit_and_insert(client, text);
                true
            }
            Action::Revert => {
                self.ivars().composer.borrow_mut().revert();
                self.show_typing(client)
            }
            Action::DeleteBack => {
                let deleted = self.ivars().composer.borrow_mut().backspace();
                if deleted {
                    self.request_rerank(false);
                    self.show_typing(client);
                }
                deleted
            }
        }
    }

    /// 選択中の候補を表示し、候補ウィンドウを出してハイライトを合わせる。
    fn show_selection(&self, client: &Client) -> bool {
        self.refresh_marked(client);
        // IMKCandidates は candidates: / candidateSelectionChanged: を同期で呼び戻し、composer を再借用する。
        // ここでは借用を持っていない。
        let Phase::Converting { selected, .. } = self.phase() else {
            return true;
        };
        self.ivars().syncing.set(true);
        if platform::show_candidates() {
            self.ivars().panel_line.set(0);
        }
        let delta = selected as isize - self.ivars().panel_line.get() as isize;
        platform::move_candidate_highlight(delta);
        self.ivars().panel_line.set(selected);
        self.ivars().syncing.set(false);
        trace::record("select", &format!("line={selected} moved={delta}"));
        true
    }

    /// 入力途中の表示を更新して候補ウィンドウを閉じる。
    fn show_typing(&self, client: &Client) -> bool {
        self.refresh_marked(client);
        self.hide_candidates_window();
        true
    }

    fn rerank_job(&self) -> Option<RerankJob> {
        let composer = self.ivars().composer.borrow();
        (composer.phase() == Phase::Composing
            && !composer.is_symbol()
            && composer.candidates().len() >= 2).then(|| {
            RerankJob {
                generation: composer.current_generation(),
                hiragana: composer.display_hiragana().to_string(),
                candidates: composer.candidates().iter().map(|c| c.text.clone()).collect(),
            }
        })
    }

    /// urgent は Space 押下時用。デバウンスを待たずに送る(送信済みなら重複しない)。
    fn request_rerank(&self, urgent: bool) {
        if !rerank_allowed(ai_rerank_enabled(), credentials_missing()) {
            return;
        }
        let Some(job) = self.rerank_job() else { return };
        let client = self.client();
        let preceding = client
            .as_ref()
            .map(|c| c.preceding_text(CONTEXT_MAX_UTF16))
            .unwrap_or_default();
        let bundle_id = client.as_ref().and_then(Client::bundle_id);
        let snapshot = Snapshot {
            secure_input: platform::is_secure_input_enabled(),
            bundle_id: bundle_id.as_deref(),
            preceding: &preceding,
        };
        let context = match outbound::decide(&snapshot) {
            Outbound::Skip(reason) => {
                platform::log(&format!("LLM rerank skipped: {}", reason.name()));
                return;
            }
            Outbound::WithoutContext(reason) => {
                platform::log(&format!("LLM rerank without context: {}", reason.name()));
                String::new()
            }
            Outbound::WithContext(context) => context,
        };
        llm_bridge::rerank_async(
            self.ivars().controller_id,
            job.generation,
            &job.hiragana,
            &context,
            &job.candidates,
            urgent,
        );
    }

    fn handle_llm_result(&self, generation: u64, status: i32, text: &str) {
        if status == llm_bridge::STATUS_CREDENTIALS_MISSING {
            set_credentials_missing(true);
        }
        if status != llm_bridge::STATUS_OK {
            platform::log(&format!("LLM rerank error status={}", status));
            return;
        }
        let Ok(index) = text.trim().parse::<usize>() else {
            return;
        };
        let Some(client) = self.client() else { return };
        let applied = self
            .ivars()
            .composer
            .borrow_mut()
            .apply_rerank(generation, index);
        if applied.is_some() {
            self.refresh_marked(&client);
        }
    }

    fn hide_candidates_window(&self) {
        platform::hide_candidates();
    }
}

pub fn register_controller() {
    let _ = NovakeyRInputController::class();
    platform::log(&format!(
        "Registered input controller class: {}",
        NovakeyRInputController::NAME
    ));
}

/// 入力メソッドとして起動し、run loop に入る。
pub fn run() {
    platform::run_application("me.sijis.inputmethod.NovakeyR_Connection");
}

const CONTEXT_MAX_UTF16: usize = 200;

static PLAYFUL_MAP: Lazy<HashMap<&'static str, Vec<&'static str>>> = Lazy::new(|| {
    let mut outs = HashMap::new();
    outs.insert("l", vec!["l", "I", "|"]);
    outs.insert("1", vec!["l", "1", "I"]);
    outs.insert("I", vec!["l", "I", "|"]);
    outs.insert("O", vec!["O", "0"]);
    outs.insert("0", vec!["O", "0"]);
    outs
});

fn playful_convert(text: &str) -> Option<String> {
    let list = PLAYFUL_MAP.get(text)?;
    let i = rand::thread_rng().gen_range(0..list.len());
    Some(list[i].to_string())
}

#[cfg(test)]
mod tests {
    use super::rerank_allowed;

    #[test]
    fn rerank_never_runs_when_ai_toggle_is_off() {
        assert!(!rerank_allowed(false, false));
        assert!(!rerank_allowed(false, true));
    }

    #[test]
    fn rerank_is_blocked_when_credentials_are_missing() {
        assert!(!rerank_allowed(true, true));
        assert!(rerank_allowed(true, false));
    }
}
