// llm_bridge.rs - Swift shim (LLMBridge.swift) への FFI。
// 契約: novakey_llm_on_result は Swift 側が main queue 上で呼ぶ。
use std::slice;
use std::str;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    Available,
    CredentialsMissing,
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
        ctx_ptr: *const u8,
        ctx_len: usize,
    );
}

pub fn availability() -> Availability {
    // SAFETY: 引数なしで状態コードを返す Swift 側の @_cdecl 関数。
    match unsafe { novakey_llm_availability() } {
        0 => Availability::Available,
        4 => Availability::CredentialsMissing,
        _ => Availability::Unknown,
    }
}

pub fn rerank_async(controller_id: u64, generation: u64, reading: &str, context: &str, candidates: &[String]) {
    if candidates.len() < 2 {
        return;
    }
    let joined = candidates.join("\u{1F}");
    // SAFETY: 各 (ptr, len) は呼び出し中生存する &str / String を指す。Swift 側は呼び出し中にコピーして保持しない。
    unsafe {
        novakey_llm_rerank(
            controller_id,
            generation,
            reading.as_ptr(),
            reading.len(),
            joined.as_ptr(),
            joined.len(),
            context.as_ptr(),
            context.len(),
        );
    }
}

type ResultHandler = fn(controller_id: u64, generation: u64, status: i32, text: &str);

static RESULT_HANDLER: OnceLock<ResultHandler> = OnceLock::new();

/// 結果ハンドラを登録する。最初の 1 回だけ有効。
pub fn set_result_handler(f: ResultHandler) {
    let _ = RESULT_HANDLER.set(f);
}

#[no_mangle]
pub extern "C" fn novakey_llm_on_result(
    controller_id: u64,
    generation: u64,
    status: i32,
    text_ptr: *const u8,
    text_len: usize,
) {
    let Some(handler) = RESULT_HANDLER.get() else {
        return;
    };
    let text = if text_ptr.is_null() || text_len == 0 {
        ""
    } else {
        // SAFETY: Swift 側は (text_ptr, text_len) がこのコールバック中有効なバイト列であることを保証する (契約)。
        let bytes = unsafe { slice::from_raw_parts(text_ptr, text_len) };
        str::from_utf8(bytes).unwrap_or("")
    };
    handler(controller_id, generation, status, text);
}

static SMOKE_START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

pub fn smoke_test() {
    println!("availability: {:?}", availability());
    if availability() != Availability::Available {
        return;
    }
    set_result_handler(|_, _, status, text| {
        println!("status={} text={} elapsed={}ms", status, text, SMOKE_START.get().map_or(0, |t| t.elapsed().as_millis()));
        std::process::exit(if status == STATUS_OK { 0 } else { 1 });
    });
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(15));
        eprintln!("timeout: no terminal status within 15s");
        std::process::exit(2);
    });
    let candidates = vec![
        "今日は医者に行く".to_string(),
        "今日歯医者に行く".to_string(),
    ];
    let _ = SMOKE_START.set(std::time::Instant::now());
    rerank_async(0, 0, "きょうはいしゃにいく", "", &candidates);
    crate::platform::run_main_run_loop();
}
