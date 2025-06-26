use cocoa::base::{id, nil, BOOL, NO, YES};
use cocoa::foundation::NSString;
use objc::declare::ClassDecl;
use objc::runtime::{Object, Sel};

use rand::Rng;
use std::collections::HashMap;
use std::{slice, str};

#[link(name = "InputMethodKit", kind = "framework")]
extern "C" {}

#[link(name = "Foundation", kind = "framework")]
extern "C" {
    pub fn NSLog(fmt: id, ...);
}

macro_rules! NSLog {
    ( $fmt:expr ) => {
        NSLog(NSString::alloc(nil).init_str($fmt))
    };
    ( $fmt:expr, $( $x:expr ),* ) => {
        NSLog(NSString::alloc(nil).init_str($fmt), $($x, )*)
    };
}

const UTF8_ENCODING: libc::c_uint = 4;

// TODO: create trait IMKServer
pub unsafe fn connect_imkserver(name: id /* NSString */, identifer: id /* NSString */) {
    let server_alloc: id = msg_send![class!(IMKServer), alloc];
    let _server: id = msg_send![server_alloc, initWithName:name bundleIdentifier:identifer];
}

pub fn register_controller() {
    let super_class = class!(IMKInputController);
    let mut decl = ClassDecl::new("NovakeyRInputController", super_class).unwrap();

    unsafe {
        decl.add_method(
            sel!(inputText:client:),
            input_text as extern "C" fn(&Object, Sel, id, id) -> BOOL,
        );
    }
    decl.register();
}

extern "C" fn input_text(_this: &Object, _cmd: Sel, text: id, sender: id) -> BOOL {
    if let Some(desc_str) = to_s(text) {
        unsafe {
            // Log the input character using NSLog with %{public}s to avoid <private>
            NSLog!("Alphabet input: %{public}s", NSString::alloc(nil).init_str(desc_str));

            // For alphabet characters, output as-is
            if desc_str.chars().all(|c| c.is_ascii_alphabetic()) {
                let _: () = msg_send![sender, insertText: text];
            } else if let Some(converted) = convert(desc_str) {
                let converted_nsstring = NSString::alloc(nil).init_str(&converted);
                let _: () = msg_send![sender, insertText: converted_nsstring];
            } else {
                let _: () = msg_send![sender, insertText: text];
            }
        }
        return YES;
    }
    return NO;
}

fn convert(text: &str) -> Option<String> {
    let mut rng = rand::thread_rng();
    let mut outs = HashMap::new();
    outs.insert("l", vec!["l", "I", "|"]);
    outs.insert("1", vec!["l", "1", "I"]);
    outs.insert("I", vec!["l", "I", "|"]);
    outs.insert("O", vec!["O", "0"]);
    outs.insert("0", vec!["O", "0"]);
    outs.insert(" ", vec![" ", "　"]);

    if let Some(list) = outs.get(text) {
        let i: i32 = rng.gen_range(0..list.len() as i32);
        return Some(list[i as usize].to_string());
    }
    return None;
}

/// Get and print an objects description
pub unsafe fn describe(obj: *mut Object) {
    let description: *mut Object = msg_send![obj, description];
    if let Some(desc_str) = to_s(description) {
        NSLog!("Object description: %{public}s", NSString::alloc(nil).init_str(desc_str));
    }
}

/// Convert an NSString to a String
fn to_s<'a>(nsstring_obj: *mut Object) -> Option<&'a str> {
    let bytes = unsafe {
        let length = msg_send![nsstring_obj, lengthOfBytesUsingEncoding: UTF8_ENCODING];
        let utf8_str: *const u8 = msg_send![nsstring_obj, UTF8String];
        slice::from_raw_parts(utf8_str, length)
    };
    str::from_utf8(bytes).ok()
}
