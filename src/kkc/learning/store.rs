//! 学習の中身 (読み → エントリ列のマップ) と、それに対する操作。ファイルや時計には触れない。

use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};
use std::ops::Deref;
use std::sync::Arc;

use crate::kkc::elem::count_graphemes;

/// 共有できる文字列。メインスレッドの記憶と書き出しスレッドの写しが、同じ読み・表層を 1 つの領域で持てるようにする。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Text(Arc<str>);

impl Text {
    #[cfg(test)]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Deref for Text {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

// `Hash` が `str` と同じ値になる (Arc<str> の Hash は str のもの) ので、`HashMap<Text, _>` を `&str` で引ける。
impl Borrow<str> for Text {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl From<&str> for Text {
    fn from(s: &str) -> Self {
        Self(Arc::from(s))
    }
}

impl From<String> for Text {
    fn from(s: String) -> Self {
        Self(Arc::from(s))
    }
}

impl PartialEq<str> for Text {
    fn eq(&self, other: &str) -> bool {
        &*self.0 == other
    }
}

impl PartialEq<&str> for Text {
    fn eq(&self, other: &&str) -> bool {
        &*self.0 == *other
    }
}

/// 学習エントリ 1 件。`ruby` は呼び出し側 (マップのキー) が持つ。
#[derive(Debug, Clone, PartialEq)]
pub struct MemEntry {
    pub word: Text,
    pub lcid: usize,
    pub rcid: usize,
    pub mid: usize,
    pub count: u8,
    pub last_used: u16,
    pub last_updated: u16,
}

impl MemEntry {
    pub(super) fn is_key(&self, word: &str, lcid: usize, rcid: usize) -> bool {
        self.word == *word && self.lcid == lcid && self.rcid == rcid
    }
}

/// 読み → その読みのエントリ列。
pub type Entries = HashMap<Text, Vec<MemEntry>>;

/// 学習エントリの一意キー (読み, 表層, lcid, rcid)。
pub type EntryKey = (Text, Text, usize, usize);

pub fn count_entries(entries: &Entries) -> usize {
    entries.values().map(Vec::len).sum()
}

/// 1 件を丸ごと入れ替える (なければ追加)。追加したときだけ true。
pub fn upsert(entries: &mut Entries, ruby: Text, entry: MemEntry) -> bool {
    let list = entries.entry(ruby).or_default();
    match list.iter_mut().find(|m| m.is_key(&entry.word, entry.lcid, entry.rcid)) {
        Some(slot) => {
            *slot = entry;
            false
        }
        None => {
            list.push(entry);
            true
        }
    }
}

/// 未来の日付のリセット・128 日以上使っていないものの削除・32 日ごとの半減。消した件数を返す。
pub fn prune_entries(entries: &mut Entries, today: u16) -> usize {
    let today = i32::from(today);
    let mut removed = 0;
    entries.retain(|_, list| {
        list.retain_mut(|m| {
            if today < i32::from(m.last_updated) || today < i32::from(m.last_used) {
                // 変なデータは入れ直す
                m.count = 1;
                m.last_used = today as u16;
                m.last_updated = today as u16;
            }
            if today - i32::from(m.last_used) >= 128 {
                removed += 1;
                return false;
            }
            while today - i32::from(m.last_updated) > 32 {
                m.count >>= 1;
                m.last_updated += 32;
            }
            if m.count == 0 {
                removed += 1;
                return false;
            }
            true
        });
        !list.is_empty()
    });
    removed
}

pub fn remove_keys(entries: &mut Entries, keys: &[EntryKey]) {
    for (ruby, word, lcid, rcid) in keys {
        if let Some(list) = entries.get_mut(ruby) {
            list.retain(|m| !m.is_key(word, *lcid, *rcid));
            if list.is_empty() {
                entries.remove(ruby);
            }
        }
    }
}

/// 上限を超えた `excess` 件のうち捨てるものを、読みの長いものから選ぶ (`touched` は選ばない)。
/// 比べるのは参照のままで、複製するのは選ばれたキーだけ。
pub fn pick_evictions(entries: &Entries, touched: &HashSet<EntryKey>, excess: usize) -> Vec<EntryKey> {
    // 長い順 (同じ長さなら読み・表層の降順) に並べて先頭から捨てる
    type Candidate<'a> = (usize, &'a Text, &'a Text, usize, usize);
    let mut keys: Vec<Candidate> = Vec::with_capacity(count_entries(entries));
    for (ruby, list) in entries {
        let ruby_len = count_graphemes(ruby);
        for m in list {
            // `touched` を引くための組は Arc の参照カウントを増やすだけで、文字列は複製しない
            if touched.is_empty() || !touched.contains(&(ruby.clone(), m.word.clone(), m.lcid, m.rcid)) {
                keys.push((ruby_len, ruby, &m.word, m.lcid, m.rcid));
            }
        }
    }
    let excess = excess.min(keys.len());
    if excess < keys.len() {
        keys.select_nth_unstable_by(excess, |a, b| b.cmp(a));
        keys.truncate(excess);
    }
    keys.into_iter().map(|(_, ruby, word, l, r)| (ruby.clone(), word.clone(), l, r)).collect()
}
