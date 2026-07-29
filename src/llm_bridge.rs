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
    fn novakey_llm_rerank(
        controller_id: u64,
        generation: u64,
        reading_ptr: *const u8,
        reading_len: usize,
        cands_ptr: *const u8,
        cands_len: usize,
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

pub fn rerank_async(controller_id: u64, generation: u64, reading: &str, candidates: &[String]) {
    if candidates.len() < 2 {
        return;
    }
    let joined = candidates.join("\u{1F}");
    unsafe {
        novakey_llm_rerank(
            controller_id,
            generation,
            reading.as_ptr(),
            reading.len(),
            joined.as_ptr(),
            joined.len(),
        );
    }
}

type ResultHandler = fn(controller_id: u64, generation: u64, status: i32, text: &str);

static RESULT_HANDLER: AtomicUsize = AtomicUsize::new(0);

pub fn set_result_handler(f: ResultHandler) {
    RESULT_HANDLER.store(f as usize, Ordering::SeqCst);
}

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

pub fn smoke_test() {
    println!("availability: {:?}", availability());
    if availability() != Availability::Available {
        return;
    }
    set_result_handler(|_, _, status, text| {
        println!("status={} text={}", status, text);
        std::process::exit(if status == STATUS_OK { 0 } else { 1 });
    });
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(15));
        eprintln!("timeout: no terminal status within 15s");
        std::process::exit(2);
    });
    let candidates = vec![
        "今日はいい天気ですね".to_string(),
        "京は良い天気ですね".to_string(),
        "今日はいい転機ですね".to_string(),
    ];
    rerank_async(0, 0, "きょうはいいてんきですね", &candidates);
    unsafe {
        use objc2::runtime::AnyObject;
        use objc2::{class, msg_send};
        let run_loop: *mut AnyObject = msg_send![class!(NSRunLoop), mainRunLoop];
        let _: () = msg_send![run_loop, run];
    }
}
