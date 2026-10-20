use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use objc2::rc::{Allocated, Id};
use objc2::runtime::{AnyObject, Bool};
use objc2::{
    declare_class, extern_class, msg_send_id, mutability, sel, ClassType, DeclaredClass,
};
use objc2_foundation::{NSArray, NSObject, NSString};

use crate::composer::{Composer, Phase};
use crate::imk_client::Client;
use crate::keymap::{self, Key, Stroke};
use crate::kkc::RustEngine;
use crate::llm_bridge;
use crate::outbound::{self, Outbound, Snapshot};
use crate::platform;
use crate::session::{Effect, Outcome, RerankJob, Session};
use crate::trace;

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
    session: RefCell<Session<RustEngine>>,
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
                session: RefCell::new(Session::new(
                    Composer::new(RustEngine::new())
                        .with_english(crate::loanword::english_for)
                        .with_emoji(crate::emoji::emoji_for),
                )),
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
            let translit = (!event.is_command_or_option)
                .then(|| {
                    keymap::translit_form(
                        event.key_code,
                        event.is_control,
                        &event.characters_ignoring_modifiers,
                    )
                })
                .flatten();
            if translit.is_none() && (event.is_control || event.is_command_or_option) {
                trace::record("handleEvent", "key=Shortcut action=PassThrough");
                return Bool::NO;
            }
            let client = Client::new(client);
            *self.ivars().last_client.borrow_mut() = Some(client.clone());
            if let Some(form) = translit {
                return Bool::new(self.dispatch(Key::Translit(form), "", &client));
            }
            let handled = match keymap::stroke(event.key_code, &event.characters, event.is_shift) {
                Stroke::Keys(keys) => keys.iter().fold(false, |handled, (key, text)| {
                    self.dispatch(*key, text, &client) || handled
                }),
                Stroke::Navigation => {
                    let has_pending = {
                        let session = self.ivars().session.borrow();
                        !session.converter.buffer().is_empty() || !session.composer.is_empty()
                    };
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
            let session = self.ivars().session.borrow();
            let texts: Vec<Id<NSString>> = session
                .composer
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
                let mut session = self.ivars().session.borrow_mut();
                session
                    .composer
                    .select_candidate_text(&text)
                    .then(|| session.composer.display().to_string())
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
            let effects = self.ivars().session.borrow_mut().commit_selected(&text);
            self.run(effects, Some(&client));
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
            let marked = {
                let mut session = self.ivars().session.borrow_mut();
                session.composer.set_live(new_mode == ConversionMode::Live);
                (!session.composer.is_empty()).then(|| session.marked_text())
            };
            if let (Some(client), Some(marked)) = (self.client(), marked) {
                client.set_marked(&marked);
            }
        }

        #[method(commitComposition:)]
        fn commit_composition(&self, client: Option<&AnyObject>) {
            let client = client.map(Client::new);
            self.sync_rerank_flag();
            let effects = self.ivars().session.borrow_mut().commit_composition();
            self.run(effects, client.as_ref());
        }
    }
);

impl NovakeyRInputController {
    fn client(&self) -> Option<Client> {
        self.ivars().last_client.borrow().clone()
    }

    fn phase(&self) -> Phase {
        self.ivars().session.borrow().phase()
    }

    /// 無効時は RequestRerank が作られず、Keychain に触れる経路へ進まない。
    fn sync_rerank_flag(&self) {
        self.ivars().session.borrow_mut().rerank =
            rerank_allowed(ai_rerank_enabled(), credentials_missing());
    }

    /// キー処理の入口。session に判断させ、返ってきた Effect を実行する。処理したら true。
    fn dispatch(&self, key: Key, text: &str, client: &Client) -> bool {
        self.sync_rerank_flag();
        // session の借用はここで外れる。以降の Cocoa 呼び出しは composer を再借用してよい。
        let outcome = self.ivars().session.borrow_mut().handle(key, text);
        let Outcome { handled, action, phase, effects } = outcome;
        self.run(effects, Some(client));
        match action {
            None => trace::record(
                "handleEvent",
                &format!("key={} phase={phase:?} action=Romaji handled={handled}", key.name()),
            ),
            Some(action) => trace::record(
                "handleEvent",
                &format!(
                    "key={} phase={phase:?} action={} handled={handled} now={:?}",
                    key.name(),
                    action.name(),
                    self.phase()
                ),
            ),
        }
        handled
    }

    fn run(&self, effects: Vec<Effect>, client: Option<&Client>) {
        for effect in effects {
            match effect {
                Effect::SetMarked(text) => {
                    if let Some(client) = client {
                        client.set_marked(&text);
                    }
                }
                Effect::Insert(text) => {
                    if let Some(client) = client {
                        client.insert(&text);
                    }
                }
                Effect::ShowCandidates { selected } => self.show_candidates(selected),
                Effect::HideCandidates => platform::hide_candidates(),
                Effect::RequestRerank { urgent, job } => self.send_rerank(urgent, &job),
            }
        }
    }

    /// 候補ウィンドウを出してハイライトを合わせる。
    fn show_candidates(&self, selected: usize) {
        // IMKCandidates は candidates: / candidateSelectionChanged: を同期で呼び戻し、session を再借用する。
        // ここでは借用を持っていない。
        self.ivars().syncing.set(true);
        if platform::show_candidates() {
            self.ivars().panel_line.set(0);
        }
        let delta = selected as isize - self.ivars().panel_line.get() as isize;
        platform::move_candidate_highlight(delta);
        self.ivars().panel_line.set(selected);
        self.ivars().syncing.set(false);
        trace::record("select", &format!("line={selected} moved={delta}"));
    }

    /// urgent は Space 押下時用。デバウンスを待たずに送る(送信済みなら重複しない)。
    fn send_rerank(&self, urgent: bool, job: &RerankJob) {
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
        let marked = {
            let mut session = self.ivars().session.borrow_mut();
            session
                .composer
                .apply_rerank(generation, index)
                .map(|_| session.marked_text())
        };
        if let Some(marked) = marked {
            client.set_marked(&marked);
        }
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
