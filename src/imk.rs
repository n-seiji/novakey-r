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

// Global state for romaji buffer
static mut ROMAJI_BUFFER: String = String::new();

extern "C" fn input_text(_this: &Object, _cmd: Sel, text: id, sender: id) -> BOOL {
    if let Some(desc_str) = to_s(text) {
        unsafe {
            NSLog!("Input received: %{public}s", NSString::alloc(nil).init_str(desc_str));

            // Handle space key - convert buffered romaji to hiragana
            if desc_str == " " {
                if !ROMAJI_BUFFER.is_empty() {
                    let romaji_map = romaji_to_hiragana();
                    if let Some(&hiragana) = romaji_map.get(ROMAJI_BUFFER.as_str()) {
                        let hiragana_nsstring = NSString::alloc(nil).init_str(hiragana);
                        let _: () = msg_send![sender, insertText: hiragana_nsstring];
                        ROMAJI_BUFFER.clear();
                    } else {
                        // If no match, output the romaji buffer as is
                        let romaji_nsstring = NSString::alloc(nil).init_str(&ROMAJI_BUFFER);
                        let _: () = msg_send![sender, insertText: romaji_nsstring];
                        ROMAJI_BUFFER.clear();
                        // Then insert the space
                        let _: () = msg_send![sender, insertText: text];
                    }
                } else {
                    let _: () = msg_send![sender, insertText: text];
                }
                return YES;
            }

            // Handle backspace - remove from buffer
            if desc_str.len() == 1 && desc_str.chars().next().unwrap() as u32 == 8 { // backspace
                if !ROMAJI_BUFFER.is_empty() {
                    ROMAJI_BUFFER.pop();
                }
                return YES;
            }

            // For ASCII alphabetic characters, add to romaji buffer
            if desc_str.chars().all(|c| c.is_ascii_alphabetic()) {
                ROMAJI_BUFFER.push_str(desc_str);
                
                // Try to match current buffer to romaji patterns
                let romaji_map = romaji_to_hiragana();
                
                // Check for exact match
                if let Some(&hiragana) = romaji_map.get(ROMAJI_BUFFER.as_str()) {
                    let hiragana_nsstring = NSString::alloc(nil).init_str(hiragana);
                    let _: () = msg_send![sender, insertText: hiragana_nsstring];
                    ROMAJI_BUFFER.clear();
                    return YES;
                }
                
                // Check if buffer could potentially match something longer
                let has_potential_match = romaji_map.keys().any(|key| key.starts_with(&ROMAJI_BUFFER));
                
                if !has_potential_match {
                    // No potential match, output what we have and start fresh
                    let romaji_nsstring = NSString::alloc(nil).init_str(&ROMAJI_BUFFER);
                    let _: () = msg_send![sender, insertText: romaji_nsstring];
                    ROMAJI_BUFFER.clear();
                }
                
                return YES;
            }

            // For non-alphabetic characters, first flush buffer then handle normally
            if !ROMAJI_BUFFER.is_empty() {
                let romaji_nsstring = NSString::alloc(nil).init_str(&ROMAJI_BUFFER);
                let _: () = msg_send![sender, insertText: romaji_nsstring];
                ROMAJI_BUFFER.clear();
            }

            // Apply original character conversion for special characters
            if let Some(converted) = convert(desc_str) {
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
    
    // Original character conversion mappings
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

fn romaji_to_hiragana() -> HashMap<&'static str, &'static str> {
    let mut map = HashMap::new();
    
    // Single vowels
    map.insert("a", "あ");
    map.insert("i", "い");
    map.insert("u", "う");
    map.insert("e", "え");
    map.insert("o", "お");
    
    // Ka row
    map.insert("ka", "か");
    map.insert("ki", "き");
    map.insert("ku", "く");
    map.insert("ke", "け");
    map.insert("ko", "こ");
    
    // Sa row
    map.insert("sa", "さ");
    map.insert("si", "し");
    map.insert("shi", "し");
    map.insert("su", "す");
    map.insert("se", "せ");
    map.insert("so", "そ");
    
    // Ta row
    map.insert("ta", "た");
    map.insert("ti", "ち");
    map.insert("chi", "ち");
    map.insert("tu", "つ");
    map.insert("tsu", "つ");
    map.insert("te", "て");
    map.insert("to", "と");
    
    // Na row
    map.insert("na", "な");
    map.insert("ni", "に");
    map.insert("nu", "ぬ");
    map.insert("ne", "ね");
    map.insert("no", "の");
    
    // Ha row
    map.insert("ha", "は");
    map.insert("hi", "ひ");
    map.insert("hu", "ふ");
    map.insert("fu", "ふ");
    map.insert("he", "へ");
    map.insert("ho", "ほ");
    
    // Ma row
    map.insert("ma", "ま");
    map.insert("mi", "み");
    map.insert("mu", "む");
    map.insert("me", "め");
    map.insert("mo", "も");
    
    // Ya row
    map.insert("ya", "や");
    map.insert("yu", "ゆ");
    map.insert("yo", "よ");
    
    // Ra row
    map.insert("ra", "ら");
    map.insert("ri", "り");
    map.insert("ru", "る");
    map.insert("re", "れ");
    map.insert("ro", "ろ");
    
    // Wa row
    map.insert("wa", "わ");
    map.insert("wo", "を");
    map.insert("n", "ん");
    
    map
}

fn hiragana_to_katakana(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            'あ' => 'ア', 'い' => 'イ', 'う' => 'ウ', 'え' => 'エ', 'お' => 'オ',
            'か' => 'カ', 'き' => 'キ', 'く' => 'ク', 'け' => 'ケ', 'こ' => 'コ',
            'さ' => 'サ', 'し' => 'シ', 'す' => 'ス', 'せ' => 'セ', 'そ' => 'ソ',
            'た' => 'タ', 'ち' => 'チ', 'つ' => 'ツ', 'て' => 'テ', 'と' => 'ト',
            'な' => 'ナ', 'に' => 'ニ', 'ぬ' => 'ヌ', 'ね' => 'ネ', 'の' => 'ノ',
            'は' => 'ハ', 'ひ' => 'ヒ', 'ふ' => 'フ', 'へ' => 'ヘ', 'ほ' => 'ホ',
            'ま' => 'マ', 'み' => 'ミ', 'む' => 'ム', 'め' => 'メ', 'も' => 'モ',
            'や' => 'ヤ', 'ゆ' => 'ユ', 'よ' => 'ヨ',
            'ら' => 'ラ', 'り' => 'リ', 'る' => 'ル', 'れ' => 'レ', 'ろ' => 'ロ',
            'わ' => 'ワ', 'を' => 'ヲ', 'ん' => 'ン',
            _ => c,
        })
        .collect()
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
