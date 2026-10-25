//! `learning.tsv` の形式と、そのファイルの読み書き (一時ファイル → rename、壊れたファイルの退避)。

use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use super::store::{Entries, MemEntry, Text};

/// File format ID, not a product name: it must stay byte for byte so that learning data
/// written before the rename to Shizuku still loads.
pub const HEADER: &str = "# novakey-r learning v1";
pub const FILE_NAME: &str = "learning.tsv";

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// `learning.tsv` に書く。pid と通し番号で一意にした一時ファイルへ書き、`sync_all` してから rename する
/// (電源断でも、半端なファイルが本物に置き換わらない)。背景スレッドから呼ぶ。
/// IME プロセスが同時に複数いると後から書いたものが勝つ (last-writer-wins) が、通常は 1 つなので許容する。
pub fn write_file(path: &Path, entries: &Entries) {
    let result = (|| -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            crate::private_fs::create_dir_all(dir)?;
        }
        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = path.with_extension(format!("tsv.tmp-{}-{n}", std::process::id()));
        let written = (|| -> std::io::Result<()> {
            let mut file = crate::private_fs::create_file(&tmp)?;
            file.write_all(serialize(entries).as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&tmp, path)
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        written
    })();
    if let Err(e) = result {
        crate::platform::log(&format!("learning file not saved: {e}"));
    }
}

/// 壊れたファイルを `<name>.corrupt-<unix 秒>` に残す。同名があれば `-1`, `-2` ... を付けて、前の退避は上書きしない。
/// `rename` が false なら写しを取る (元のファイルは次の保存で置き換わる)。
pub fn backup(path: &Path, rename: bool) -> std::io::Result<()> {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let name = path.file_name().map_or_else(|| FILE_NAME.into(), |n| n.to_string_lossy().into_owned());
    let mut target = path.with_file_name(format!("{name}.corrupt-{secs}"));
    let mut n = 0;
    while target.exists() {
        n += 1;
        target = path.with_file_name(format!("{name}.corrupt-{secs}-{n}"));
    }
    if rename {
        std::fs::rename(path, target)
    } else {
        crate::private_fs::copy(path, &target)
    }
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

fn unescape(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        out.push(match chars.next()? {
            '\\' => '\\',
            't' => '\t',
            'n' => '\n',
            'r' => '\r',
            _ => return None,
        });
    }
    Some(out)
}

/// 読み → 表層 → lcid → rcid の順に並べて書く (内容が同じなら同じバイト列になる)。
pub fn serialize(entries: &Entries) -> String {
    let mut rubies: Vec<&Text> = entries.keys().collect();
    rubies.sort();
    let mut out = String::from(HEADER);
    out.push('\n');
    for ruby in rubies {
        let mut list: Vec<&MemEntry> = entries[ruby].iter().collect();
        list.sort_by(|a, b| (&a.word, a.lcid, a.rcid).cmp(&(&b.word, b.lcid, b.rcid)));
        for m in list {
            out.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                escape(ruby),
                escape(&m.word),
                m.lcid,
                m.rcid,
                m.mid,
                m.count,
                m.last_used,
                m.last_updated
            ));
        }
    }
    out
}

pub struct Parsed {
    pub entries: Entries,
    /// 読めずに飛ばした行数 (空行は数えない)
    pub skipped: usize,
}

/// 1 行ずつ読み、読めない行・重複する行は飛ばして数える。ヘッダが違う (別のファイル・壊れている) ときだけ `None`。
pub fn parse(bytes: &[u8]) -> Option<Parsed> {
    let mut lines = bytes.split(|&b| b == b'\n');
    let header = std::str::from_utf8(lines.next()?).ok()?;
    if header.trim_end_matches('\r') != HEADER {
        return None;
    }
    let mut entries = Entries::new();
    let mut skipped = 0;
    for raw in lines {
        if raw.is_empty() {
            continue;
        }
        let Some((ruby, m)) = std::str::from_utf8(raw).ok().and_then(|line| parse_line(line.trim_end_matches('\r'))) else {
            skipped += 1;
            continue;
        };
        let list = entries.entry(ruby).or_default();
        if list.iter().any(|x| x.is_key(&m.word, m.lcid, m.rcid)) {
            skipped += 1;
            continue;
        }
        list.push(m);
    }
    Some(Parsed { entries, skipped })
}

fn parse_line(line: &str) -> Option<(Text, MemEntry)> {
    let f: Vec<&str> = line.split('\t').collect();
    if f.len() != 8 {
        return None;
    }
    let ruby = unescape(f[0])?;
    if ruby.is_empty() {
        return None;
    }
    let m = MemEntry {
        word: unescape(f[1])?.into(),
        lcid: f[2].parse().ok()?,
        rcid: f[3].parse().ok()?,
        mid: f[4].parse().ok()?,
        count: f[5].parse().ok()?,
        last_used: f[6].parse().ok()?,
        last_updated: f[7].parse().ok()?,
    };
    Some((ruby.into(), m))
}

#[cfg(test)]
mod header_tests {
    use super::HEADER;

    #[test]
    fn header_is_the_format_id_and_never_follows_the_product_name() {
        assert_eq!(HEADER, "# novakey-r learning v1");
    }
}
