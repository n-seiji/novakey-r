use std::ffi::{c_char, CStr};

extern "C" {
    fn novakey_kkc_convert(
        controller_id: u64,
        revision: u64,
        reading_ptr: *const u8,
        reading_len: usize,
    ) -> *mut c_char;
    fn novakey_kkc_select(controller_id: u64, revision: u64, engine_index: isize) -> bool;
    fn novakey_kkc_cancel(controller_id: u64);
}

pub fn convert(controller_id: u64, revision: u64, reading: &str) -> Vec<String> {
    if reading.is_empty() {
        return Vec::new();
    }
    // SAFETY: (ptr, len) は reading の有効なバイト列。
    let ptr = unsafe { novakey_kkc_convert(controller_id, revision, reading.as_ptr(), reading.len()) };
    let joined = take_c_string(ptr);
    if joined.is_empty() {
        return Vec::new();
    }
    joined.split('\u{1F}').map(str::to_string).collect()
}

/// Swift 側が malloc した NUL 終端文字列を String に写して解放する。null は空文字列。
fn take_c_string(ptr: *mut c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    // SAFETY: ptr は novakey_kkc_convert が返した、malloc 済みで NUL 終端の非 null 文字列。一度だけ読んで free する。
    unsafe {
        let s = CStr::from_ptr(ptr).to_string_lossy().into_owned();
        libc::free(ptr as *mut libc::c_void);
        s
    }
}

pub fn select(controller_id: u64, revision: u64, engine_index: usize) -> bool {
    // SAFETY: 引数はすべて値渡しの整数。
    unsafe { novakey_kkc_select(controller_id, revision, engine_index as isize) }
}

pub fn cancel(controller_id: u64) {
    // SAFETY: 引数は値渡しの整数。
    unsafe { novakey_kkc_cancel(controller_id) };
}

/// 読みを省略するとスモーク用の既定の文で、変換結果のアサートまで行う。読みを渡したときは候補を表示するだけ。
pub fn smoke_test(reading: Option<&str>) {
    let memory_dir = std::env::temp_dir()
        .join(format!("novakey-r-kkc-smoke-{}", std::process::id()));
    std::env::set_var("NOVAKEY_DISABLE_LEARNING", "1");
    std::env::set_var("NOVAKEY_MEMORY_DIR", &memory_dir);

    let default = reading.is_none();
    let reading = reading.unwrap_or("きょうはいいてんきですね");
    let candidates = convert(0, 1, reading);
    println!("candidates: {}", candidates.len());
    for (i, c) in candidates.iter().enumerate() {
        println!("{}: {}", i, c);
    }
    if !default {
        cancel(0);
        return;
    }
    assert!(select(0, 1, 0), "select が revision 一致で失敗");
    let again = convert(0, 2, "きょう");
    assert!(!again.is_empty());
    assert!(!select(0, 99, 0), "select が revision 不一致なのに成功");
    cancel(0);
    assert!(!candidates.is_empty(), "候補が空");
    assert!(
        candidates.iter().any(|c| c.contains("天気")),
        "天気を含む候補がない: {:?}",
        candidates
    );
    println!("OK");
}
