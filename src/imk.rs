use std::cell::RefCell;
use std::collections::HashMap;
use std::{slice, str};

use objc2::rc::{Allocated, Id};
use objc2::runtime::{AnyObject, Bool, Sel};
use objc2::{
    declare_class, extern_class, msg_send, msg_send_id, mutability, sel, ClassType, DeclaredClass,
};
use objc2_foundation::{NSNotFound, NSObject, NSRange, NSString};
use once_cell::sync::Lazy;
use rand::Rng;

use crate::composer::{CandidateEngine, Composer};
use crate::kkc_bridge;
use crate::llm_bridge;
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
    static SERVER: std::cell::Cell<*mut AnyObject> = const { std::cell::Cell::new(std::ptr::null_mut()) };
    static CANDIDATES_PANEL: std::cell::Cell<*mut AnyObject> = const { std::cell::Cell::new(std::ptr::null_mut()) };
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
        unsafe {
            let msg = NSString::from_str(&format!("LLM unavailable: {:?}", avail));
            NSLog(msg.as_ref() as *const NSString as *mut AnyObject);
        }
    }
}

fn llm_enabled() -> bool {
    LLM_ENABLED.with(|e| e.get())
}

#[link(name = "InputMethodKit", kind = "framework")]
extern "C" {}

#[link(name = "AppKit", kind = "framework")]
extern "C" {}

#[link(name = "Foundation", kind = "framework")]
extern "C" {
    pub fn NSLog(fmt: *mut AnyObject, ...);
}

macro_rules! NSLog {
    ( $fmt:expr ) => {{
        let ns_str = NSString::from_str($fmt);
        NSLog(ns_str.as_ref() as *const NSString as *mut AnyObject)
    }};
    ( $fmt:expr, $( $x:expr ),* ) => {{
        let ns_str = NSString::from_str($fmt);
        NSLog(ns_str.as_ref() as *const NSString as *mut AnyObject, $($x, )*)
    }};
}

const UTF8_ENCODING: libc::c_uint = 4;
const K_IMK_SINGLE_COLUMN_SCROLLING_PANEL: usize = 1;
const K_IMK_LOCATE_CANDIDATES_BELOW_HINT: usize = 2;

extern_class!(
    #[derive(Debug, PartialEq, Eq, Hash)]
    pub struct IMKServer;

    unsafe impl ClassType for IMKServer {
        type Super = NSObject;
        type Mutability = mutability::InteriorMutable;
        const NAME: &'static str = "IMKServer";
    }
);

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
    last_client: RefCell<*mut AnyObject>,
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
                last_client: RefCell::new(std::ptr::null_mut()),
            });
            let this: Option<Id<Self>> = unsafe {
                msg_send_id![super(this), initWithServer: server, delegate: delegate, client: client]
            };
            if let Some(ref obj) = this {
                CONTROLLERS.with(|c| c.borrow_mut().insert(controller_id, obj.clone()));
            }
            this
        }

        #[method(inputText:client:)]
        fn input_text(&self, text: *mut AnyObject, client: *mut AnyObject) -> Bool {
            let Some(input) = to_s(text) else {
                return Bool::NO;
            };
            *self.ivars().last_client.borrow_mut() = client;
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
                        unsafe { insert_str(client, &converted) };
                        return Bool::YES;
                    }
                    return Bool::NO;
                }
                if composer.cycle() {
                    let marked = composer.display().to_string();
                    unsafe { set_marked_str(client, &marked) };
                    self.update_candidates_window(&composer);
                } else {
                    let marked = composer.display().to_string();
                    unsafe { set_marked_str(client, &marked) };
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
                unsafe { set_marked_str(client, &marked) };
                self.hide_candidates_window();
                return Bool::YES;
            }

            let romaji_out = converter.flush();
            drop(converter);
            let mut composer = self.ivars().composer.borrow_mut();
            if let Some(out) = romaji_out {
                composer.push_hiragana(&out);
            }
            if composer.is_empty() {
                if let Some(converted) = playful_convert(input) {
                    unsafe { insert_str(client, &converted) };
                    return Bool::YES;
                }
                return Bool::NO;
            }
            composer.push_hiragana(input);
            self.request_rerank(&composer);
            let marked = composer.display().to_string();
            unsafe { set_marked_str(client, &marked) };
            self.hide_candidates_window();
            Bool::YES
        }

        #[method(didCommandBySelector:client:)]
        fn did_command_by_selector(&self, selector: Sel, client: *mut AnyObject) -> Bool {
            *self.ivars().last_client.borrow_mut() = client;
            let mut converter = self.ivars().converter.borrow_mut();

            if selector == sel!(deleteBackward:) {
                if !converter.buffer().is_empty() {
                    converter.handle_backspace();
                    let romaji = converter.buffer().to_string();
                    drop(converter);
                    let composer = self.ivars().composer.borrow();
                    let marked = format!("{}{}", composer.display(), romaji);
                    unsafe { set_marked_str(client, &marked) };
                    return Bool::YES;
                }
                drop(converter);
                let mut composer = self.ivars().composer.borrow_mut();
                if !composer.backspace() {
                    return Bool::NO;
                }
                self.request_rerank(&composer);
                let marked = composer.display().to_string();
                unsafe { set_marked_str(client, &marked) };
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
                unsafe {
                    set_marked_str(client, "");
                    insert_str(client, &text);
                }
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
                unsafe { set_marked_str(client, &marked) };
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


        #[method(candidates:)]
        fn candidates(&self, _sender: *mut AnyObject) -> *mut AnyObject {
            let composer = self.ivars().composer.borrow();
            unsafe {
                let array: *mut AnyObject = msg_send![objc2::class!(NSMutableArray), new];
                for cand in composer.candidates() {
                    let ns = NSString::from_str(&cand.text);
                    let _: () = msg_send![array, addObject: &*ns];
                }
                msg_send![array, autorelease]
            }
        }

        #[method(candidateSelectionChanged:)]
        fn candidate_selection_changed(&self, candidate_string: *mut AnyObject) {
            let client = *self.ivars().last_client.borrow();
            if client.is_null() {
                return;
            }
            let text: *mut AnyObject = unsafe { msg_send![candidate_string, string] };
            let Some(text) = to_s(text) else { return };
            let mut composer = self.ivars().composer.borrow_mut();
            if let Some(idx) = composer.candidates().iter().position(|c| c.text == text) {
                while composer.selected_index() != Some(idx) {
                    if !composer.cycle() {
                        return;
                    }
                }
                let marked = composer.display().to_string();
                unsafe { set_marked_str(client, &marked) };
            }
        }

        #[method(candidateSelected:)]
        fn candidate_selected(&self, candidate_string: *mut AnyObject) {
            let client = *self.ivars().last_client.borrow();
            if client.is_null() {
                return;
            }
            let text: *mut AnyObject = unsafe { msg_send![candidate_string, string] };
            let Some(text) = to_s(text) else { return };
            let mut composer = self.ivars().composer.borrow_mut();
            if let Some(idx) = composer.candidates().iter().position(|c| c.text == text) {
                while composer.selected_index() != Some(idx) {
                    if !composer.cycle() {
                        break;
                    }
                }
            }
            let out = composer.commit();
            unsafe {
                set_marked_str(client, "");
                insert_str(client, &out);
            }
            self.hide_candidates_window();
        }

        #[method(menu)]
        fn menu(&self) -> *mut AnyObject {
            unsafe {
                let menu: *mut AnyObject = msg_send![objc2::class!(NSMenu), new];
                let title = NSString::from_str(match *self.ivars().mode.borrow() {
                    ConversionMode::Live => "ライブ変換: オン",
                    ConversionMode::Space => "ライブ変換: オフ(スペースで変換)",
                });
                let item: *mut AnyObject = msg_send![objc2::class!(NSMenuItem), alloc];
                let item: *mut AnyObject = msg_send![
                    item,
                    initWithTitle: &*title,
                    action: sel!(toggleConversionMode:),
                    keyEquivalent: &*NSString::from_str("")
                ];
                let _: () = msg_send![menu, addItem: item];
                if !llm_enabled() {
                    let warn = NSString::from_str("LLMリランキング無効(Apple Intelligence を確認)");
                    let warn_item: *mut AnyObject = msg_send![objc2::class!(NSMenuItem), new];
                    let _: () = msg_send![warn_item, setTitle: &*warn];
                    let _: () = msg_send![warn_item, setEnabled: Bool::NO];
                    let _: () = msg_send![menu, addItem: warn_item];
                }
                menu
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
            let mut composer = self.ivars().composer.borrow_mut();
            composer.set_live(new_mode == ConversionMode::Live);
            let client = *self.ivars().last_client.borrow();
            if !client.is_null() && !composer.is_empty() {
                let marked = composer.display().to_string();
                unsafe { set_marked_str(client, &marked) };
            }
        }

        #[method(commitComposition:)]
        fn commit_composition(&self, client: *mut AnyObject) {
            let mut converter = self.ivars().converter.borrow_mut();
            let romaji_out = converter.flush();
            drop(converter);
            let mut composer = self.ivars().composer.borrow_mut();
            if let Some(out) = romaji_out {
                composer.push_hiragana(&out);
            }
            if !composer.is_empty() {
                let text = composer.commit();
                unsafe { insert_str(client, &text) };
            }
            self.hide_candidates_window();
        }
    }
);

impl NovakeyRInputController {
    fn request_rerank(&self, composer: &Composer<KkcEngine>) {
        if !llm_enabled() || composer.candidates().len() < 2 || composer.is_raw() {
            return;
        }
        let candidates: Vec<String> = composer.candidates().iter().map(|c| c.text.clone()).collect();
        llm_bridge::rerank_async(
            self.ivars().controller_id,
            composer.current_generation(),
            composer.display_hiragana(),
            &candidates,
        );
    }

    fn handle_llm_result(&self, generation: u64, status: i32, text: &str) {
        if status != llm_bridge::STATUS_OK {
            unsafe { NSLog!("LLM rerank error status=%d", status) };
            return;
        }
        let Ok(index) = text.trim().parse::<usize>() else {
            return;
        };
        let client = *self.ivars().last_client.borrow();
        if client.is_null() || !self.ivars().converter.borrow().buffer().is_empty() {
            return;
        }
        let mut composer = self.ivars().composer.borrow_mut();
        if let Some(marked) = composer.apply_rerank(generation, index) {
            unsafe { set_marked_str(client, &marked) };
        }
    }

    fn update_candidates_window(&self, composer: &Composer<KkcEngine>) {
        let panel = unsafe { candidates_panel() };
        if panel.is_null() || composer.candidates().is_empty() {
            return;
        }
        unsafe {
            let _: () = msg_send![panel, updateCandidates];
            let _: () = msg_send![panel, show: K_IMK_LOCATE_CANDIDATES_BELOW_HINT];
        }
    }

    fn hide_candidates_window(&self) {
        CANDIDATES_PANEL.with(|p| {
            let panel = p.get();
            if !panel.is_null() {
                unsafe {
                    let _: () = msg_send![panel, hide];
                }
            }
        });
    }
}

pub fn register_controller() {
    let _ = NovakeyRInputController::class();
    unsafe {
        NSLog!(
            "Registered input controller class: %@",
            NSString::from_str(NovakeyRInputController::NAME).as_ref() as *const NSString
                as *mut AnyObject
        );
    }
}

pub unsafe fn connect_imkserver(name: *mut AnyObject, identifer: *mut AnyObject) {
    let server_class = IMKServer::class();
    let server_alloc: *mut AnyObject = msg_send![server_class, alloc];
    let server: *mut AnyObject =
        msg_send![server_alloc, initWithName: name, bundleIdentifier: identifer];
    SERVER.with(|s| s.set(server));
    NSLog!("IMKServer connection established");
}

unsafe fn candidates_panel() -> *mut AnyObject {
    CANDIDATES_PANEL.with(|cell| {
        let existing = cell.get();
        if !existing.is_null() {
            return existing;
        }
        let server = SERVER.with(|s| s.get());
        if server.is_null() {
            return std::ptr::null_mut();
        }
        let panel: *mut AnyObject = msg_send![objc2::class!(IMKCandidates), alloc];
        let panel: *mut AnyObject = msg_send![
            panel,
            initWithServer: server,
            panelType: K_IMK_SINGLE_COLUMN_SCROLLING_PANEL
        ];
        cell.set(panel);
        panel
    })
}

unsafe fn insert_str(client: *mut AnyObject, text: &str) {
    let ns = NSString::from_str(text);
    let replacement_range = NSRange {
        location: NSNotFound as usize,
        length: 0,
    };
    let _: () = msg_send![client, insertText: &*ns, replacementRange: replacement_range];
}

unsafe fn set_marked_str(client: *mut AnyObject, text: &str) {
    let ns = NSString::from_str(text);
    let length: usize = msg_send![&*ns, length];
    let selection_range = NSRange { location: length, length: 0 };
    let replacement_range = NSRange {
        location: NSNotFound as usize,
        length: 0,
    };
    let _: () = msg_send![
        client,
        setMarkedText: &*ns,
        selectionRange: selection_range,
        replacementRange: replacement_range
    ];
}

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

pub unsafe fn describe(obj: *mut AnyObject) {
    let description: *mut AnyObject = msg_send![obj, description];
    if let Some(desc_str) = to_s(description) {
        NSLog!(
            "Object description: %@",
            NSString::from_str(desc_str).as_ref() as *const NSString as *mut AnyObject
        );
    }
}

fn to_s<'a>(nsstring_obj: *mut AnyObject) -> Option<&'a str> {
    let bytes = unsafe {
        let length: usize = msg_send![nsstring_obj, lengthOfBytesUsingEncoding: UTF8_ENCODING];
        let utf8_str: *const u8 = msg_send![nsstring_obj, UTF8String];
        slice::from_raw_parts(utf8_str, length)
    };
    str::from_utf8(bytes).ok()
}
