#![forbid(unsafe_code)]

//! 調査用のキー操作トレース。`defaults write dev.n-seiji.inputmethod.NovakeyR NovakeyRTrace -bool true` で有効になり、
//! ~/Library/Logs/NovakeyR/trace.log に 1 行ずつ追記する。入力文字や候補の文字列は書かない。
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::platform;

const TRACE_KEY: &str = "NovakeyRTrace";

/// fields は "名前=値" のように内容を含まない語だけを渡すこと。
pub fn record(entry: &str, fields: &str) {
    if !platform::defaults_bool(TRACE_KEY) {
        return;
    }
    let Some(dir) = std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Logs/NovakeyR"))
    else {
        return;
    };
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let line = format!("{}.{:03} {entry} {fields}\n", now.as_secs(), now.subsec_millis());
    let written = crate::private_fs::create_dir_all(&dir)
        .and_then(|()| crate::private_fs::open_append(&dir.join("trace.log"))?.write_all(line.as_bytes()));
    if let Err(err) = written {
        platform::log(&format!("trace write failed: {err}"));
    }
}
