// llm_bridge.rs - Swift shim (LLMBridge.swift) への FFI。
// 契約: novakey_llm_on_result は Swift 側が main queue 上で呼ぶ。
use std::slice;
use std::str;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    Available,
    DeviceNotEligible,
    NotEnabled,
    ModelNotReady,
    Unknown,
}

pub const STATUS_OK: i32 = 0;

extern "C" {
    fn novakey_llm_availability() -> i32;
    fn novakey_llm_convert(
        controller_id: u64,
        generation: u64,
        input_ptr: *const u8,
        input_len: usize,
        ctx_ptr: *const u8,
        ctx_len: usize,
    );
}

pub fn availability() -> Availability {
    match unsafe { novakey_llm_availability() } {
        0 => Availability::Available,
        1 => Availability::DeviceNotEligible,
        2 => Availability::NotEnabled,
        3 => Availability::ModelNotReady,
        _ => Availability::Unknown,
    }
}

pub fn convert_async(controller_id: u64, generation: u64, hiragana: &str, context: &str) {
    unsafe {
        novakey_llm_convert(
            controller_id,
            generation,
            hiragana.as_ptr(),
            hiragana.len(),
            context.as_ptr(),
            context.len(),
        );
    }
}

type ResultHandler = fn(controller_id: u64, generation: u64, status: i32, text: &str);

// fn ポインタを usize で保持(メインスレッドでのみ読み書きされるが、
// 型として Sync が必要なため atomic を使う)
static RESULT_HANDLER: AtomicUsize = AtomicUsize::new(0);

pub fn set_result_handler(f: ResultHandler) {
    RESULT_HANDLER.store(f as usize, Ordering::SeqCst);
}

/// Swift 側から main queue 上で呼ばれる。
#[no_mangle]
pub extern "C" fn novakey_llm_on_result(
    controller_id: u64,
    generation: u64,
    status: i32,
    text_ptr: *const u8,
    text_len: usize,
) {
    let handler = RESULT_HANDLER.load(Ordering::SeqCst);
    if handler == 0 {
        return;
    }
    let text = if text_ptr.is_null() || text_len == 0 {
        ""
    } else {
        let bytes = unsafe { slice::from_raw_parts(text_ptr, text_len) };
        str::from_utf8(bytes).unwrap_or("")
    };
    let f: ResultHandler = unsafe { std::mem::transmute(handler) };
    f(controller_id, generation, status, text);
}

/// mise run test-llm 用: availability を表示し1回変換して終了。
pub fn smoke_test() {
    println!("availability: {:?}", availability());
    if availability() != Availability::Available {
        return;
    }
    set_result_handler(|_, _, status, text| {
        println!("status={} text={}", status, text);
        std::process::exit(0);
    });
    convert_async(0, 0, "きょうはいいてんきですね", "");
    // main queue のコールバックを受けるため run loop を回す
    unsafe {
        use objc2::runtime::AnyObject;
        use objc2::{class, msg_send};
        let run_loop: *mut AnyObject = msg_send![class!(NSRunLoop), mainRunLoop];
        let _: () = msg_send![run_loop, run];
    }
}
