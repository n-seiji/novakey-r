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
use crate::llm_bridge::{Choice, Delivery, RerankService, Unavailable};
use crate::platform;
use crate::registry::Registry;
use crate::outbound::Screened;
use crate::rerank::Availability;
use crate::rerank_gate::AiStatus;
use crate::session::ConversionMode;
use crate::trace;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
static NEXT_CONTROLLER_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static CONTROLLERS: Registry<WeakId<NovakeyRInputController>> = Registry::new();
    static AI_STATUS: std::cell::Cell<AiStatus> = const { std::cell::Cell::new(AiStatus::Off) };
}

const AI_RERANK_KEY: &str = "NovakeyRAIRerankEnabled";

/// AI 並べ替えの送信側。プロセスに 1 つだけ、最初の送信かトグル ON で起こす。
/// グローバルなのは、IMK のコールバック (Environment を作るのは Cocoa 側) から届く経路がほかにないため。
static RERANK: OnceLock<Arc<RerankService>> = OnceLock::new();

fn rerank_service() -> &'static Arc<RerankService> {
    RERANK.get_or_init(|| RerankService::start(dispatch_llm_result))
}

/// まだ起こしていなければ何もしない。
fn cancel_rerank(controller_id: u64) {
    if let Some(service) = RERANK.get() {
        service.cancel(controller_id);
    }
}

/// 起こしていなければ、使える (読み出し中・未読込と同じ) とみなす。
fn rerank_availability() -> Availability {
    RERANK.get().map_or(Availability::Ready, |service| service.availability())
}

fn dispatch_llm_result(delivery: Delivery) {
    CONTROLLERS.with(|c| {
        c.deliver(delivery.controller_id, |weak| weak.load(), |controller| {
            controller.handle_llm_result(delivery.generation, &delivery.outcome);
        });
    });
}

pub fn init_llm_dispatch() {
    set_ai_status(if platform::defaults_bool(AI_RERANK_KEY) { AiStatus::On(Availability::Ready) } else { AiStatus::Off });
}

fn ai_status() -> AiStatus {
    AI_STATUS.with(|e| e.get())
}

fn set_ai_status(status: AiStatus) {
    AI_STATUS.with(|e| e.set(status));
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
        // 一時停止が明けた・認証情報の読み直しが済んだことを拾うため、毎回送信側の状態を取り込む (ロックだけで Keychain には触れない)。
        set_ai_status(ai_status().observe(rerank_availability()));
        ai_status().allows_requests()
    }

    fn log(&self, line: &str) {
        platform::log(line);
    }

    fn trace(&self, entry: &str, fields: &str) {
        trace::record(entry, fields);
    }

    fn send_rerank(&self, controller_id: u64, generation: u64, payload: &Screened, urgent: bool) {
        rerank_service().submit(controller_id, generation, payload.clone(), urgent);
    }

    fn cancel_rerank(&self, controller_id: u64) {
        cancel_rerank(controller_id);
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
            cancel_rerank(self.id);
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
                let ai_title = if ai_status().is_on() {
                    "AI並べ替え: オン"
                } else {
                    "AI並べ替え: オフ"
                };
                set_ai_status(ai_status().observe(rerank_availability()));
                let warning = ai_status().warning();
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
                let enabled = !ai_status().is_on();
                set_ai_status(if enabled { AiStatus::On(Availability::Ready) } else { AiStatus::Off });
                platform::set_defaults_bool(AI_RERANK_KEY, enabled);
                if !enabled {
                    if let Some(service) = RERANK.get() {
                        service.cancel_all();
                    }
                }
                if enabled {
                    // Keychain の読み出しは止まりうるので待たない。結果は、メニューを開いたとき (menu) か
                    // 最初の送信の結果で警告に反映される。
                    rerank_service().prepare();
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

    fn handle_llm_result(&self, generation: u64, outcome: &Result<Choice, Unavailable>) {
        self.guard_session(|| (), || self.apply_llm_result(generation, outcome));
    }

    fn apply_llm_result(&self, generation: u64, outcome: &Result<Choice, Unavailable>) {
        if !ai_status().accepts_results() {
            return;
        }
        match outcome {
            Ok(choice) => self.ivars().controller.on_rerank_result(generation, choice),
            Err(reason) => {
                if *reason == Unavailable::CredentialsMissing {
                    set_ai_status(ai_status().observe(Availability::CredentialsMissing));
                }
                platform::log(&format!("LLM rerank error: {reason:?}"));
            }
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

#[cfg(test)]
mod tests {
    use objc2::ClassType;

    use super::NovakeyRInputController;

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
}
