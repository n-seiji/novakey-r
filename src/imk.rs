use std::cell::RefCell;
use std::collections::HashMap;

use objc2::rc::{Allocated, Id};
use objc2::runtime::{AnyObject, Bool, Sel};
use objc2::{
    declare_class, extern_class, msg_send_id, mutability, sel, ClassType, DeclaredClass,
};
use objc2_foundation::{NSArray, NSObject, NSString};
use once_cell::sync::Lazy;
use rand::Rng;

use crate::composer::{CandidateEngine, Composer};
use crate::imk_client::Client;
use crate::kkc_bridge;
use crate::llm_bridge;
use crate::platform;
use crate::romaji_converter::RomajiConverter;

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
    static LLM_ENABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

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
    llm_bridge::set_result_handler(dispatch_llm_result);
    let avail = llm_bridge::availability();
    if avail != llm_bridge::Availability::Available {
        LLM_ENABLED.with(|e| e.set(false));
        platform::log(&format!("LLM unavailable: {:?}", avail));
    }
}

fn llm_enabled() -> bool {
    LLM_ENABLED.with(|e| e.get())
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
                mode: RefCell::new(ConversionMode::Live),
                controller_id,
                last_client: RefCell::new(None),
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

        #[method(inputText:client:)]
        fn input_text(&self, text: Option<&NSString>, client: Option<&AnyObject>) -> Bool {
            let (Some(text), Some(client)) = (text, client) else {
                return Bool::NO;
            };
            let input = text.to_string();
            let client = Client::new(client);
            *self.ivars().last_client.borrow_mut() = Some(client.clone());
            let mut converter = self.ivars().converter.borrow_mut();

            if input == " " {
                let romaji_out = converter.flush();
                drop(converter);
                let mut composer = self.ivars().composer.borrow_mut();
                if let Some(out) = romaji_out {
                    composer.push_hiragana(&out);
                    self.request_rerank(&composer);
                }
                if composer.is_empty() {
                    if let Some(converted) = playful_convert(" ") {
                        client.insert(&converted);
                        return Bool::YES;
                    }
                    return Bool::NO;
                }
                if composer.cycle() {
                    let marked = composer.display().to_string();
                    client.set_marked(&marked);
                    self.update_candidates_window(&composer);
                } else {
                    let marked = composer.display().to_string();
                    client.set_marked(&marked);
                }
                return Bool::YES;
            }

            if !input.is_empty() && input.chars().all(|c| c.is_ascii_alphabetic()) {
                let mut kana = String::new();
                for ch in input.chars() {
                    if let Some(output) = converter.process_input(ch) {
                        kana.push_str(&output);
                    }
                }
                let romaji = converter.buffer().to_string();
                drop(converter);
                let mut composer = self.ivars().composer.borrow_mut();
                if !kana.is_empty() {
                    composer.push_hiragana(&kana);
                    self.request_rerank(&composer);
                }
                let marked = format!("{}{}", composer.display(), romaji);
                client.set_marked(&marked);
                self.hide_candidates_window();
                return Bool::YES;
            }

            let romaji_out = converter.flush();
            drop(converter);
            let mut composer = self.ivars().composer.borrow_mut();
            if let Some(out) = romaji_out {
                composer.push_hiragana(&out);
            }
            let punctuation = japanese_punctuation(&input);
            if composer.is_empty() {
                if let Some(converted) = punctuation {
                    client.insert(converted);
                    return Bool::YES;
                }
                if let Some(converted) = playful_convert(&input) {
                    client.insert(&converted);
                    return Bool::YES;
                }
                return Bool::NO;
            }
            composer.push_hiragana(punctuation.unwrap_or(&input));
            self.request_rerank(&composer);
            let marked = composer.display().to_string();
            client.set_marked(&marked);
            self.hide_candidates_window();
            Bool::YES
        }

        #[method(didCommandBySelector:client:)]
        fn did_command_by_selector(&self, selector: Sel, client: Option<&AnyObject>) -> Bool {
            let Some(client) = client.map(Client::new) else {
                return Bool::NO;
            };
            *self.ivars().last_client.borrow_mut() = Some(client.clone());
            let mut converter = self.ivars().converter.borrow_mut();

            if selector == sel!(deleteBackward:) {
                if !converter.buffer().is_empty() {
                    converter.handle_backspace();
                    let romaji = converter.buffer().to_string();
                    drop(converter);
                    let composer = self.ivars().composer.borrow();
                    let marked = format!("{}{}", composer.display(), romaji);
                    client.set_marked(&marked);
                    return Bool::YES;
                }
                drop(converter);
                let mut composer = self.ivars().composer.borrow_mut();
                if !composer.backspace() {
                    return Bool::NO;
                }
                self.request_rerank(&composer);
                let marked = composer.display().to_string();
                client.set_marked(&marked);
                self.hide_candidates_window();
                return Bool::YES;
            }

            if selector == sel!(insertNewline:) {
                if let Some(output) = converter.flush() {
                    drop(converter);
                    self.ivars().composer.borrow_mut().push_hiragana(&output);
                } else {
                    drop(converter);
                }
                let mut composer = self.ivars().composer.borrow_mut();
                if composer.is_empty() {
                    return Bool::NO;
                }
                let text = composer.commit();
                client.set_marked("");
                client.insert(&text);
                self.hide_candidates_window();
                return Bool::YES;
            }

            if selector == sel!(cancelOperation:) {
                converter.clear();
                drop(converter);
                let mut composer = self.ivars().composer.borrow_mut();
                if composer.is_empty() {
                    return Bool::NO;
                }
                if composer.is_raw() {
                    return Bool::NO;
                }
                composer.revert_raw();
                let marked = composer.display().to_string();
                client.set_marked(&marked);
                self.hide_candidates_window();
                return Bool::YES;
            }

            if selector == sel!(moveLeft:) || selector == sel!(moveRight:) {
                let has_pending = !converter.buffer().is_empty()
                    || !self.ivars().composer.borrow().is_empty();
                return if has_pending { Bool::YES } else { Bool::NO };
            }

            Bool::NO
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
            let Some(client) = self.client() else { return };
            let Some(text) = candidate_string.and_then(platform::attributed_text) else {
                return;
            };
            let mut composer = self.ivars().composer.borrow_mut();
            if let Some(idx) = composer.candidates().iter().position(|c| c.text == text) {
                while composer.selected_index() != Some(idx) {
                    if !composer.cycle() {
                        return;
                    }
                }
                let marked = composer.display().to_string();
                client.set_marked(&marked);
            }
        }

        #[method(candidateSelected:)]
        fn candidate_selected(&self, candidate_string: Option<&AnyObject>) {
            let Some(client) = self.client() else { return };
            let Some(text) = candidate_string.and_then(platform::attributed_text) else {
                return;
            };
            let mut composer = self.ivars().composer.borrow_mut();
            if let Some(idx) = composer.candidates().iter().position(|c| c.text == text) {
                while composer.selected_index() != Some(idx) {
                    if !composer.cycle() {
                        break;
                    }
                }
            }
            let out = composer.commit();
            client.set_marked("");
            client.insert(&out);
            self.hide_candidates_window();
        }

        #[method_id(menu)]
        fn menu(&self) -> Id<AnyObject> {
            let mode_title = match *self.ivars().mode.borrow() {
                ConversionMode::Live => "ライブ変換: オン",
                ConversionMode::Space => "ライブ変換: オフ(スペースで変換)",
            };
            let warning = (!llm_enabled()).then_some("LLMリランキング無効(Cloudflare認証情報を確認)");
            platform::build_menu(mode_title, warning)
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
            let mut composer = self.ivars().composer.borrow_mut();
            composer.set_live(new_mode == ConversionMode::Live);
            if let (Some(client), false) = (self.client(), composer.is_empty()) {
                let marked = composer.display().to_string();
                client.set_marked(&marked);
            }
        }

        #[method(commitComposition:)]
        fn commit_composition(&self, client: Option<&AnyObject>) {
            let client = client.map(Client::new);
            let mut converter = self.ivars().converter.borrow_mut();
            let romaji_out = converter.flush();
            drop(converter);
            let mut composer = self.ivars().composer.borrow_mut();
            if let Some(out) = romaji_out {
                composer.push_hiragana(&out);
            }
            if let (Some(client), false) = (client, composer.is_empty()) {
                let text = composer.commit();
                client.insert(&text);
            }
            self.hide_candidates_window();
        }
    }
);

impl NovakeyRInputController {
    fn client(&self) -> Option<Client> {
        self.ivars().last_client.borrow().clone()
    }

    fn request_rerank(&self, composer: &Composer<KkcEngine>) {
        if !llm_enabled() || composer.candidates().len() < 2 || composer.is_raw() {
            return;
        }
        let candidates: Vec<String> = composer.candidates().iter().map(|c| c.text.clone()).collect();
        let context = self
            .client()
            .map(|c| c.preceding_text(CONTEXT_MAX_UTF16))
            .unwrap_or_default();
        llm_bridge::rerank_async(
            self.ivars().controller_id,
            composer.current_generation(),
            composer.display_hiragana(),
            &context,
            &candidates,
        );
    }

    fn handle_llm_result(&self, generation: u64, status: i32, text: &str) {
        if status != llm_bridge::STATUS_OK {
            platform::log(&format!("LLM rerank error status={}", status));
            return;
        }
        let Ok(index) = text.trim().parse::<usize>() else {
            return;
        };
        let Some(client) = self.client() else { return };
        if !self.ivars().converter.borrow().buffer().is_empty() {
            return;
        }
        let mut composer = self.ivars().composer.borrow_mut();
        if let Some(marked) = composer.apply_rerank(generation, index) {
            client.set_marked(&marked);
        }
    }

    fn update_candidates_window(&self, composer: &Composer<KkcEngine>) {
        if !composer.candidates().is_empty() {
            platform::show_candidates();
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
    outs.insert(" ", vec![" ", "　"]);
    outs
});

fn playful_convert(text: &str) -> Option<String> {
    let list = PLAYFUL_MAP.get(text)?;
    let i = rand::thread_rng().gen_range(0..list.len());
    Some(list[i].to_string())
}

/// IME で直接入力した ASCII 句読点を、日本語入力用の表記へ正規化する。
fn japanese_punctuation(text: &str) -> Option<&'static str> {
    match text {
        "." => Some("。"),
        "," => Some("、"),
        "-" => Some("ー"),
        "!" => Some("！"),
        "?" => Some("？"),
        "[" => Some("「"),
        "]" => Some("」"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::japanese_punctuation;

    #[test]
    fn converts_ascii_punctuation_to_japanese_forms() {
        assert_eq!(japanese_punctuation("."), Some("。"));
        assert_eq!(japanese_punctuation(","), Some("、"));
        assert_eq!(japanese_punctuation("-"), Some("ー"));
        assert_eq!(japanese_punctuation("!"), Some("！"));
        assert_eq!(japanese_punctuation("?"), Some("？"));
        assert_eq!(japanese_punctuation("["), Some("「"));
        assert_eq!(japanese_punctuation("]"), Some("」"));
        assert_eq!(japanese_punctuation("a"), None);
    }
}
