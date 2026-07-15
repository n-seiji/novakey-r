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

use crate::romaji_converter::RomajiConverter;

#[link(name = "InputMethodKit", kind = "framework")]
extern "C" {}

// NSApplication is looked up at runtime via class!, so AppKit must be linked
// explicitly; nothing else pulls it in directly.
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

// Declare IMKServer as an external class
extern_class!(
    #[derive(Debug, PartialEq, Eq, Hash)]
    pub struct IMKServer;

    unsafe impl ClassType for IMKServer {
        type Super = NSObject;
        type Mutability = mutability::InteriorMutable;
        const NAME: &'static str = "IMKServer";
    }
);

// Declare IMKInputController as an external class
extern_class!(
    #[derive(Debug, PartialEq, Eq, Hash)]
    pub struct IMKInputController;

    unsafe impl ClassType for IMKInputController {
        type Super = NSObject;
        type Mutability = mutability::InteriorMutable;
        const NAME: &'static str = "IMKInputController";
    }
);

// Per-controller state: each client (app) gets its own converter so
// composition in one app never leaks into another.
pub struct Ivars {
    converter: RefCell<RomajiConverter>,
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
            let this = this.set_ivars(Ivars {
                converter: RefCell::new(RomajiConverter::new()),
            });
            unsafe {
                msg_send_id![super(this), initWithServer: server, delegate: delegate, client: client]
            }
        }

        #[method(inputText:client:)]
        fn input_text(&self, text: *mut AnyObject, client: *mut AnyObject) -> Bool {
            let Some(input) = to_s(text) else {
                return Bool::NO;
            };
            unsafe {
                NSLog!(
                    "Input received: %@",
                    NSString::from_str(input).as_ref() as *const NSString as *mut AnyObject
                );
            }

            let mut converter = self.ivars().converter.borrow_mut();

            // Alphabetic input feeds the romaji composition; converted kana is
            // committed and the remaining romaji stays visible as marked text.
            if !input.is_empty() && input.chars().all(|c| c.is_ascii_alphabetic()) {
                let mut committed = String::new();
                for ch in input.chars() {
                    if let Some(output) = converter.process_input(ch) {
                        committed.push_str(&output);
                    }
                }
                unsafe {
                    if !committed.is_empty() {
                        insert_str(client, &committed);
                    }
                    set_marked_str(client, converter.buffer());
                }
                return Bool::YES;
            }

            // Anything else ends the composition: commit pending romaji first.
            if let Some(output) = converter.flush() {
                unsafe { insert_str(client, &output) };
            }

            // Playful conversion for select characters; otherwise let the
            // client handle the key itself.
            if let Some(converted) = playful_convert(input) {
                unsafe { insert_str(client, &converted) };
                return Bool::YES;
            }
            Bool::NO
        }

        // Non-character keys (delete, return, escape, ...) arrive here, not in
        // inputText:client:.
        #[method(didCommandBySelector:client:)]
        fn did_command_by_selector(&self, selector: Sel, client: *mut AnyObject) -> Bool {
            let mut converter = self.ivars().converter.borrow_mut();

            if selector == sel!(deleteBackward:) {
                if converter.buffer().is_empty() {
                    return Bool::NO;
                }
                converter.handle_backspace();
                unsafe { set_marked_str(client, converter.buffer()) };
                return Bool::YES;
            }

            if selector == sel!(insertNewline:) {
                // Commit pending romaji, then let the client insert the newline.
                if let Some(output) = converter.flush() {
                    unsafe { insert_str(client, &output) };
                }
                return Bool::NO;
            }

            if selector == sel!(cancelOperation:) {
                if converter.buffer().is_empty() {
                    return Bool::NO;
                }
                converter.clear();
                unsafe { set_marked_str(client, "") };
                return Bool::YES;
            }

            Bool::NO
        }

        // Called by the system when composition must end (focus change, etc.).
        #[method(commitComposition:)]
        fn commit_composition(&self, client: *mut AnyObject) {
            let mut converter = self.ivars().converter.borrow_mut();
            if let Some(output) = converter.flush() {
                unsafe { insert_str(client, &output) };
            }
        }
    }
);

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

// TODO: create trait IMKServer
pub unsafe fn connect_imkserver(name: *mut AnyObject, identifer: *mut AnyObject) {
    let server_class = IMKServer::class();
    let server_alloc: *mut AnyObject = msg_send![server_class, alloc];
    let _server: *mut AnyObject =
        msg_send![server_alloc, initWithName: name, bundleIdentifier: identifer];
    NSLog!("IMKServer connection established");
}

/// Commit text to the client, replacing any marked text.
unsafe fn insert_str(client: *mut AnyObject, text: &str) {
    let ns = NSString::from_str(text);
    let replacement_range = NSRange {
        location: NSNotFound as usize,
        length: 0,
    };
    let _: () = msg_send![client, insertText: &*ns, replacementRange: replacement_range];
}

/// Show the pending romaji as underlined marked text (empty string clears it).
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

/// Get and print an objects description
pub unsafe fn describe(obj: *mut AnyObject) {
    let description: *mut AnyObject = msg_send![obj, description];
    if let Some(desc_str) = to_s(description) {
        NSLog!(
            "Object description: %@",
            NSString::from_str(desc_str).as_ref() as *const NSString as *mut AnyObject
        );
    }
}

/// Convert an NSString to a String
fn to_s<'a>(nsstring_obj: *mut AnyObject) -> Option<&'a str> {
    let bytes = unsafe {
        let length: usize = msg_send![nsstring_obj, lengthOfBytesUsingEncoding: UTF8_ENCODING];
        let utf8_str: *const u8 = msg_send![nsstring_obj, UTF8String];
        slice::from_raw_parts(utf8_str, length)
    };
    str::from_utf8(bytes).ok()
}
