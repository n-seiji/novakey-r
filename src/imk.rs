#![allow(unsafe_code)]
use objc2::rc::{Allocated, Id};
use objc2::runtime::{AnyObject, Bool};
use objc2::{
    declare_class, extern_class, msg_send_id, mutability, sel, ClassType, DeclaredClass,
};
use objc2_foundation::{NSArray, NSObject, NSString};

use crate::controller::{new_session, CandidatePanel, Controller, Environment};
use crate::imk_client::Client;
use crate::input::{self, Route};
use crate::platform;
use crate::session::ConversionMode;
use crate::trace;

use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_CONTROLLER_ID: AtomicU64 = AtomicU64::new(1);

/// 未設定ならスペース変換 (オフ)。全コントローラで共有する。
const LIVE_CONVERSION_KEY: &str = "ShizukuLiveConversionEnabled";

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

/// 入力中 (Phase が Empty 以外) の間だけ App Nap を止める活動を持つ。コントローラと一緒に解放される。
#[derive(Default)]
struct PlatformEnv {
    activity: platform::Activity,
}

impl Environment for PlatformEnv {
    fn live_conversion(&self) -> bool {
        platform::defaults_bool(LIVE_CONVERSION_KEY)
    }

    fn set_live_conversion(&self, enabled: bool) {
        platform::set_defaults_bool(LIVE_CONVERSION_KEY, enabled);
    }

    fn secure_input(&self) -> bool {
        platform::is_secure_input_enabled()
    }

    fn set_composing(&self, composing: bool) {
        if composing {
            self.activity.begin();
        } else {
            self.activity.end();
        }
    }

    fn log(&self, line: &str) {
        platform::log(line);
    }

    fn trace(&self, entry: &str, fields: &str) {
        trace::record(entry, fields);
    }
}

pub struct Ivars {
    controller: Controller<Client, PlatformPanel, PlatformEnv>,
}

declare_class!(
    pub struct ShizukuInputController;

    unsafe impl ClassType for ShizukuInputController {
        type Super = IMKInputController;
        type Mutability = mutability::InteriorMutable;
        const NAME: &'static str = "ShizukuInputController";
    }

    impl DeclaredClass for ShizukuInputController {
        type Ivars = Ivars;
    }

    unsafe impl ShizukuInputController {
        #[method_id(initWithServer:delegate:client:)]
        fn init_with_server(
            this: Allocated<Self>,
            server: *mut AnyObject,
            delegate: *mut AnyObject,
            client: *mut AnyObject,
        ) -> Option<Id<Self>> {
            platform::guard(|| None, || {
                let controller_id = NEXT_CONTROLLER_ID.fetch_add(1, Ordering::Relaxed);
                let this = this.set_ivars(Ivars {
                    controller: Controller::new(
                        controller_id,
                        new_session(if PlatformEnv::default().live_conversion() { ConversionMode::Live } else { ConversionMode::Space }),
                        PlatformPanel,
                        PlatformEnv::default(),
                    ),
                });
                // SAFETY: 親クラスの指定イニシャライザに、IMK から受け取った引数をそのまま渡す。
                unsafe {
                    msg_send_id![super(this), initWithServer: server, delegate: delegate, client: client]
                }
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
                let started = std::time::Instant::now();
                let handled = self.ivars().controller.on_key(route, Client::new(client));
                trace::record("keyTime", &format!("ms={:.1}", started.elapsed().as_secs_f64() * 1e3));
                Bool::new(handled)
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
            self.guard_session(|| platform::build_menu(&[]), || {
                self.ivars().controller.sync_conversion_mode();
                let mode_title = match self.ivars().controller.mode() {
                    ConversionMode::Live => "ライブ変換をオフにする",
                    ConversionMode::Space => "ライブ変換をオンにする",
                };
                platform::build_menu(&[(mode_title, sel!(toggleConversionMode:))])
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

impl ShizukuInputController {
    /// session を触るコールバックを panic から守る。panic した時点で session / composer は中途半端かもしれないので、
    /// 作り直して画面側 (候補パネル・マーク文字) も揃えてから fallback を返す。
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
}

pub fn register_controller() {
    let _ = ShizukuInputController::class();
    platform::log(&format!(
        "Registered input controller class: {}",
        ShizukuInputController::NAME
    ));
}

/// Info.plist の InputMethodConnectionName が読めないとき (bundle 外の起動など) の接続名。
const CONNECTION_NAME_FALLBACK: &str = "dev.n-seiji.inputmethod.Shizuku_Connection";

/// 入力メソッドとして起動し、run loop に入る。
pub fn run() {
    let connection_name = platform::bundle_info_string("InputMethodConnectionName")
        .unwrap_or_else(|| CONNECTION_NAME_FALLBACK.to_string());
    platform::run_application(&connection_name);
}

#[cfg(test)]
mod tests {
    use objc2::ClassType;

    use super::{ShizukuInputController, CONNECTION_NAME_FALLBACK};

    #[test]
    fn connection_name_in_info_plist_matches_the_fallback() {
        let plist = include_str!("../Info.plist");
        assert!(
            plist.contains(&format!("<string>{CONNECTION_NAME_FALLBACK}</string>")),
            "Info.plist lacks the connection name {CONNECTION_NAME_FALLBACK}"
        );
        assert_eq!(
            CONNECTION_NAME_FALLBACK,
            "dev.n-seiji.inputmethod.Shizuku_Connection"
        );
    }

    #[test]
    fn the_controller_class_registers_with_imk_overrides() {
        // debug ビルドの objc2 は、上書きしたメソッドの型を IMKInputController 側と照合する。
        let _ = ShizukuInputController::class();
    }
}
