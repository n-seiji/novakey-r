#![allow(unsafe_code)]
use objc2::rc::{Allocated, Id, WeakId};
use objc2::runtime::{AnyObject, Bool};
use objc2::{
    declare_class, extern_class, msg_send_id, mutability, sel, ClassType, DeclaredClass,
};
use objc2_foundation::{NSArray, NSObject, NSString};

use crate::controller::{new_session, CandidatePanel, Controller, Environment};
use crate::imk_client::Client;
use crate::input::{self, Route};
use crate::llm_bridge;
use crate::platform;
use crate::registry::Registry;
use crate::session::{ConversionMode, RerankJob};
use crate::trace;

use std::sync::atomic::{AtomicU64, Ordering};
static NEXT_CONTROLLER_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static CONTROLLERS: Registry<WeakId<NovakeyRInputController>> = Registry::new();
    static CREDENTIALS_MISSING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static AI_RERANK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

const AI_RERANK_KEY: &str = "NovakeyRAIRerankEnabled";

fn dispatch_llm_result(controller_id: u64, generation: u64, status: i32, text: &str) {
    CONTROLLERS.with(|c| {
        c.deliver(controller_id, |weak| weak.load(), |controller| {
            controller.handle_llm_result(generation, status, text);
        });
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

/// AI をオフにした後に届いた結果は、取り消し前に配信が済んでいたものでも反映しない。
fn accepts_llm_result(ai_enabled: bool) -> bool {
    ai_enabled
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

struct PlatformPanel;

impl CandidatePanel for PlatformPanel {
    fn show(&self) -> bool {
        platform::show_candidates()
    }

    fn hide(&self) {
        platform::hide_candidates();
    }

    fn move_highlight(&self, delta: isize) {
        platform::move_candidate_highlight(delta);
    }
}

struct PlatformEnv;

impl Environment for PlatformEnv {
    fn secure_input(&self) -> bool {
        platform::is_secure_input_enabled()
    }

    fn rerank_allowed(&self) -> bool {
        rerank_allowed(ai_rerank_enabled(), credentials_missing())
    }

    fn log(&self, line: &str) {
        platform::log(line);
    }

    fn trace(&self, entry: &str, fields: &str) {
        trace::record(entry, fields);
    }

    fn send_rerank(&self, controller_id: u64, job: &RerankJob, context: &str, urgent: bool) {
        llm_bridge::rerank_async(controller_id, job.generation, &job.hiragana, context, &job.candidates, urgent);
    }

    fn cancel_rerank(&self, controller_id: u64) {
        llm_bridge::cancel(controller_id);
    }
}

pub struct Ivars {
    id: u64,
    controller: Controller<Client, PlatformPanel, PlatformEnv>,
}

/// objc2 は Ivars が Drop を持つとき dealloc を生成し、そこで drop する。登録簿から外し、AI 要求を取り消す。
impl Drop for Ivars {
    fn drop(&mut self) {
        platform::guard(|| (), || {
            let _ = CONTROLLERS.try_with(|c| c.remove(self.id));
            llm_bridge::cancel(self.id);
        });
    }
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
            platform::guard(|| None, || {
                let controller_id = NEXT_CONTROLLER_ID.fetch_add(1, Ordering::SeqCst);
                let this = this.set_ivars(Ivars {
                    id: controller_id,
                    controller: Controller::new(
                        controller_id,
                        new_session(ConversionMode::Space, 0),
                        PlatformPanel,
                        PlatformEnv,
                    ),
                });
                // SAFETY: 親クラスの指定イニシャライザに、IMK から受け取った引数をそのまま渡す。
                let this: Option<Id<Self>> = unsafe {
                    msg_send_id![super(this), initWithServer: server, delegate: delegate, client: client]
                };
                if let Some(ref obj) = this {
                    CONTROLLERS.with(|c| {
                        if !c.insert(controller_id, WeakId::from_id(obj)) {
                            platform::log("controller registry busy; AI results disabled for this controller");
                        }
                    });
                }
                this
            })
        }

        #[method(recognizedEvents:)]
        fn recognized_events(&self, _client: Option<&AnyObject>) -> usize {
            platform::guard(|| 0, || {
                platform::NS_EVENT_MASK_KEY_DOWN
            })
        }

        /// キー入力の唯一の入口。候補パネル表示中も先にここへ届く (make_panel 参照)。
        #[method(handleEvent:client:)]
        fn handle_event(&self, event: Option<&AnyObject>, client: Option<&AnyObject>) -> Bool {
            self.guard_session(|| Bool::NO, || {
                let (Some(event), Some(client)) = (event.and_then(platform::key_event), client) else {
                    return Bool::NO;
                };
                let route = input::route(&event);
                if route == Route::PassThrough {
                    trace::record("handleEvent", "key=Shortcut action=PassThrough");
                    return Bool::NO;
                }
                Bool::new(self.ivars().controller.on_key(route, Client::new(client)))
            })
        }

        #[method_id(candidates:)]
        fn candidates(&self, _sender: Option<&AnyObject>) -> Id<NSArray<NSString>> {
            platform::guard(|| NSArray::from_vec(Vec::new()), || {
                let texts: Vec<Id<NSString>> = self
                    .ivars()
                    .controller
                    .candidate_texts()
                    .iter()
                    .map(|text| NSString::from_str(text))
                    .collect();
                NSArray::from_vec(texts)
            })
        }

        #[method(candidateSelectionChanged:)]
        fn candidate_selection_changed(&self, candidate_string: Option<&AnyObject>) {
            self.guard_session(|| (), || {
                if self.ivars().controller.is_syncing() {
                    return;
                }
                let Some(text) = candidate_string.and_then(platform::candidate_text) else {
                    return;
                };
                self.ivars().controller.on_selection_changed(&text);
            })
        }

        #[method(candidateSelected:)]
        fn candidate_selected(&self, candidate_string: Option<&AnyObject>) {
            self.guard_session(|| (), || {
                let Some(text) = candidate_string.and_then(platform::candidate_text) else {
                    return;
                };
                self.ivars().controller.on_candidate_selected(&text);
            })
        }

        #[method_id(menu)]
        fn menu(&self) -> Id<AnyObject> {
            platform::guard(|| platform::build_menu(&[], None), || {
                let mode_title = match self.ivars().controller.mode() {
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
            })
        }

        #[method(toggleAIRerank:)]
        fn toggle_ai_rerank(&self, _sender: *mut AnyObject) {
            platform::guard(|| (), || {
                let enabled = !ai_rerank_enabled();
                AI_RERANK.with(|e| e.set(enabled));
                platform::set_defaults_bool(AI_RERANK_KEY, enabled);
                if !enabled {
                    llm_bridge::cancel_all();
                }
                if enabled {
                    let avail = llm_bridge::availability();
                    set_credentials_missing(avail != llm_bridge::Availability::Available);
                    if credentials_missing() {
                        platform::log(&format!("LLM unavailable: {:?}", avail));
                    }
                }
            })
        }

        #[method(toggleConversionMode:)]
        fn toggle_conversion_mode(&self, _sender: *mut AnyObject) {
            self.guard_session(|| (), || {
                self.ivars().controller.on_toggle_conversion_mode();
            })
        }

        #[method(commitComposition:)]
        fn commit_composition(&self, client: Option<&AnyObject>) {
            self.guard_session(|| (), || {
                self.ivars().controller.on_commit_composition(client.map(Client::new));
            })
        }
    }
);

impl NovakeyRInputController {
    /// session を触るコールバックを panic から守る。panic した時点で session / composer は中途半端かもしれないので、
    /// 作り直して画面側 (候補パネル・マーク文字・AI 要求) も揃えてから fallback を返す。
    fn guard_session<T>(&self, fallback: impl FnOnce() -> T, f: impl FnOnce() -> T) -> T {
        platform::guard(
            || {
                self.reset_after_panic();
                fallback()
            },
            f,
        )
    }

    fn reset_after_panic(&self) {
        // ここで再び panic すると abort するので、これ自体も守る。
        platform::guard(|| (), || self.ivars().controller.reset_after_panic());
    }

    fn handle_llm_result(&self, generation: u64, status: i32, text: &str) {
        self.guard_session(|| (), || self.apply_llm_result(generation, status, text));
    }

    fn apply_llm_result(&self, generation: u64, status: i32, text: &str) {
        if !accepts_llm_result(ai_rerank_enabled()) {
            return;
        }
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
        self.ivars().controller.on_rerank_result(generation, index);
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

#[cfg(test)]
mod tests {
    use objc2::ClassType;

    use super::{accepts_llm_result, rerank_allowed, NovakeyRInputController};

    #[test]
    fn the_controller_class_registers_with_imk_overrides() {
        // debug ビルドの objc2 は、上書きしたメソッドの型を IMKInputController 側と照合する。
        let _ = NovakeyRInputController::class();
    }

    #[test]
    fn dropping_a_controller_removes_it_from_the_registry() {
        // 実際の dealloc で Ivars の Drop が走り、登録簿から外れることの確認。
        use objc2::msg_send_id;
        use objc2::rc::Id;
        use objc2::runtime::AnyObject;

        use super::CONTROLLERS;

        let before = CONTROLLERS.with(|c| c.len());
        let null: *mut AnyObject = std::ptr::null_mut();
        // SAFETY: nil の server / delegate / client で親の指定イニシャライザを呼ぶ (IMK 本体なしのテスト用)。
        let controller: Option<Id<NovakeyRInputController>> = unsafe {
            msg_send_id![NovakeyRInputController::alloc(), initWithServer: null, delegate: null, client: null]
        };
        let controller = controller.expect("init");
        assert_eq!(CONTROLLERS.with(|c| c.len()), before + 1);
        drop(controller);
        assert_eq!(CONTROLLERS.with(|c| c.len()), before);
    }

    #[test]
    fn results_are_ignored_once_ai_is_turned_off() {
        assert!(accepts_llm_result(true));
        assert!(!accepts_llm_result(false));
    }

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
