//! ファイルへの書き出しを受け持つ背景スレッド。メイン側と同じ操作を同じ順序で受けて写しに反映し、まとめて書く。
//! 写しの読み・表層は `Text` なのでメイン側の記憶と文字列を共有し、増えるのはマップと小さな値の分だけ。

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::format::write_file;
use super::store::{prune_entries, remove_keys, upsert, Entries, EntryKey, MemEntry, Text};

/// 背景スレッドに流す差分。メイン側と同じ操作を同じ順序で流すので、写しはメインの記憶と常に同じ内容になる。
pub enum Msg {
    /// 1 件を丸ごと入れ替える (なければ追加)
    Upsert(Text, MemEntry),
    Prune(u16),
    Remove(Vec<EntryKey>),
    /// ここまでの変更を書き終えたら返事をする
    #[cfg(test)]
    Flush(Sender<()>),
}

/// 落とす (drop) と、未保存の分を書いてから止まる。
pub struct Writer {
    tx: Option<Sender<Msg>>,
    handle: Option<JoinHandle<()>>,
    /// 送れなかったことをもう記録したか (記録は 1 度だけ)
    warned: Cell<bool>,
}

impl Writer {
    pub fn spawn(path: PathBuf, entries: Entries, interval: Duration) -> Option<Self> {
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::Builder::new()
            .name("novakey-learning".into())
            .spawn(move || writer_loop(&rx, &path, entries, interval))
            .ok()?;
        Some(Self { tx: Some(tx), handle: Some(handle), warned: Cell::new(false) })
    }

    pub fn send(&self, msg: Msg) {
        let Some(tx) = &self.tx else { return };
        if tx.send(msg).is_err() && !self.warned.replace(true) {
            crate::platform::log("learning writer stopped; later changes are not saved");
        }
    }

    #[cfg(test)]
    fn with_sender(tx: Sender<Msg>) -> Self {
        Self { tx: Some(tx), handle: None, warned: Cell::new(false) }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn writer_loop(rx: &mpsc::Receiver<Msg>, path: &Path, mut entries: Entries, interval: Duration) {
    let mut dirty_since: Option<Instant> = None;
    loop {
        let received = match dirty_since {
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            Some(since) => rx.recv_timeout(interval.saturating_sub(since.elapsed())),
        };
        match received {
            #[cfg(test)]
            Ok(Msg::Flush(reply)) => {
                if dirty_since.take().is_some() {
                    write_file(path, &entries);
                }
                let _ = reply.send(());
            }
            Ok(msg) => {
                apply(&mut entries, msg);
                dirty_since.get_or_insert_with(Instant::now);
            }
            Err(RecvTimeoutError::Timeout) => {
                dirty_since = None;
                write_file(path, &entries);
            }
            Err(RecvTimeoutError::Disconnected) => {
                if dirty_since.is_some() {
                    write_file(path, &entries);
                }
                return;
            }
        }
    }
}

fn apply(entries: &mut Entries, msg: Msg) {
    match msg {
        Msg::Upsert(ruby, entry) => {
            upsert(entries, ruby, entry);
        }
        Msg::Prune(today) => {
            prune_entries(entries, today);
        }
        Msg::Remove(keys) => remove_keys(entries, &keys),
        #[cfg(test)]
        Msg::Flush(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_send_is_logged_only_once() {
        let (tx, rx) = mpsc::channel();
        let w = Writer::with_sender(tx);
        w.send(Msg::Prune(1));
        assert!(!w.warned.get());
        drop(rx);
        w.send(Msg::Prune(2));
        assert!(w.warned.get());
        w.send(Msg::Prune(3));
        assert!(w.warned.get());
    }
}
