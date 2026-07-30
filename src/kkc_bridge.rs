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
    let ptr = unsafe {
        novakey_kkc_convert(controller_id, revision, reading.as_ptr(), reading.len())
    };
    if ptr.is_null() {
        return Vec::new();
    }
    let joined = unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned();
    unsafe { libc::free(ptr as *mut libc::c_void) };
    if joined.is_empty() {
        return Vec::new();
    }
    joined.split('\u{1F}').map(str::to_string).collect()
}

pub fn select(controller_id: u64, revision: u64, engine_index: usize) -> bool {
    unsafe { novakey_kkc_select(controller_id, revision, engine_index as isize) }
}

pub fn cancel(controller_id: u64) {
    unsafe { novakey_kkc_cancel(controller_id) };
}

pub fn smoke_test() {
    let memory_dir = std::env::temp_dir()
        .join(format!("novakey-r-kkc-smoke-{}", std::process::id()));
    std::env::set_var("NOVAKEY_DISABLE_LEARNING", "1");
    std::env::set_var("NOVAKEY_MEMORY_DIR", &memory_dir);

    let candidates = convert(0, 1, "きょうはいいてんきですね");
    println!("candidates: {}", candidates.len());
    for (i, c) in candidates.iter().enumerate() {
        println!("{}: {}", i, c);
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
