//! 学習 (`LearningMemory.swift` の `LearningManager` / `LongTermLearningMemory` の移植)。
//!
//! 確定した候補を、単語ごと・第 1+第 2 文節の複合・候補全体の 3 種のエントリとして覚え、
//! 次の変換でラティスに辞書と同じように加える。Swift 版の LOUDS 形式は読まず、ruby でソートした
//! 1 つの TSV (`learning.tsv`) に書く。確定は一時記憶を経ずに即メモリへ反映し (IME は即確定なので不要)、
//! ファイルへの書き出しは背景スレッドが持つ写しに差分を流して、まとめて (最短 `FLUSH_INTERVAL` ごとに 1 回) 行う。
//!
//! Swift 版との違い:
//! - Swift は書き出すときに `lastUpdatedDay := lastUsedDay` にしてしまい、32 日ごとの半減が次の保存の
//!   たびにやり直しになる。ここでは `lastUpdatedDay` も保存して、意図どおり 32 日ごとに 1 回だけ半減する。
//! - 上限 (65536) を超えるときは、Swift と同じく読みの長いエントリから捨てる。ただし今回の確定で
//!   触れたエントリは捨てない。
//! - 直前の確定との bigram (`lastData`) は移植しない (NovakeyR は確定のたびに変換を止めるので発火しない)。
#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::elem::{count_graphemes, include_mm_value_calculation, is_clause, Elem};
use super::lattice::Found;

/// 覚えるエントリ数の上限 (`maxMemoryCount`)
pub const MAX_MEMORY_COUNT: usize = 65536;
const HEADER: &str = "# novakey-r learning v1";
const FILE_NAME: &str = "learning.tsv";
const DIR_NAME: &str = "rust-learning";
/// 最初の未保存の変更から、この時間だけ待ってまとめて書く。
const FLUSH_INTERVAL: Duration = Duration::from_millis(1500);

/// 学習エントリ 1 件。`ruby` は呼び出し側 (マップのキー) が持つ。
#[derive(Debug, Clone, PartialEq)]
pub struct MemEntry {
    pub word: String,
    pub lcid: usize,
    pub rcid: usize,
    pub mid: usize,
    pub count: u8,
    pub last_used: u16,
    pub last_updated: u16,
}

/// 値の式 (`valueForData`)。出現数が多いほど、読みが長いほど大きい。
pub fn value_for(count: u8, ruby_len: usize) -> f32 {
    let d = 1.0 - f64::from(count) / 255.0;
    (-1.0 - 4.0 / ruby_len.max(1) as f64 - 3.0 * d.powi(3)) as f32
}

/// `DicdataStore.needWValueMemory`: 単語単位で覚える語彙か。
pub fn need_w_value_memory(lcid: usize) -> bool {
    !((147..=554).contains(&lcid)
        || (557..=560).contains(&lcid)
        || (1297..=1305).contains(&lcid)
        || (6..=9).contains(&lcid)
        || lcid == 0
        || lcid == 1316)
}

/// 日付 (1970-01-01 からの日数 − 19000)。
pub fn today_from_unix(secs: u64) -> u16 {
    (secs / 86400).wrapping_sub(19000) as u16
}

fn system_today() -> u16 {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    today_from_unix(secs)
}

/// 学習データの置き場所 (`<NOVAKEY_MEMORY_DIR または ~/Library/Application Support/NovakeyR>/rust-learning`)。
pub fn default_dir() -> Option<PathBuf> {
    let base = match std::env::var("NOVAKEY_MEMORY_DIR") {
        Ok(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from(std::env::var_os("HOME")?).join("Library/Application Support/NovakeyR"),
    };
    Some(base.join(DIR_NAME))
}

/// 値のように扱う 1 回分の語 (clause の組み立て用)
#[derive(Clone)]
struct Piece {
    word: String,
    ruby: String,
    lcid: usize,
    rcid: usize,
    mid: usize,
    base: f32,
}

impl Piece {
    fn of(e: &Elem) -> Self {
        Self { word: e.word.clone(), ruby: e.ruby.clone(), lcid: e.lcid, rcid: e.rcid, mid: e.mid, base: e.base }
    }

    fn append(&mut self, e: &Elem) {
        self.word.push_str(&e.word);
        self.ruby.push_str(&e.ruby);
        self.rcid = e.rcid;
        if include_mm_value_calculation(e) {
            self.mid = e.mid;
        }
        self.base += e.base;
    }

    /// 第 1 文節 + 第 2 文節 (`mid` は第 2 文節のもの)
    fn compound(first: &Piece, second: &Piece) -> Piece {
        Piece {
            word: format!("{}{}", first.word, second.word),
            ruby: format!("{}{}", first.ruby, second.ruby),
            lcid: first.lcid,
            rcid: first.rcid,
            mid: second.mid,
            base: first.base + second.base,
        }
    }
}

/// 学習エントリの一意キー (読み, 表層, lcid, rcid)。
type EntryKey = (String, String, usize, usize);

/// 背景スレッドに流す差分。メイン側と同じ操作を同じ順序で流すので、写しはメインの記憶と常に同じ内容になる。
enum Msg {
    /// 1 件を丸ごと入れ替える (なければ追加)
    Upsert(String, MemEntry),
    Prune(u16),
    Remove(Vec<EntryKey>),
    /// ここまでの変更を書き終えたら返事をする
    Flush(Sender<()>),
}

/// ファイルへの書き出しを受け持つ背景スレッド。落とす (drop) と、未保存の分を書いてから止まる。
struct Writer {
    tx: Option<Sender<Msg>>,
    handle: Option<JoinHandle<()>>,
}

impl Writer {
    fn spawn(path: PathBuf, entries: HashMap<String, Vec<MemEntry>>, interval: Duration) -> Option<Self> {
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::Builder::new()
            .name("novakey-learning".into())
            .spawn(move || writer_loop(&rx, &path, entries, interval))
            .ok()?;
        Some(Self { tx: Some(tx), handle: Some(handle) })
    }

    fn send(&self, msg: Msg) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(msg);
        }
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

fn writer_loop(rx: &mpsc::Receiver<Msg>, path: &Path, mut entries: HashMap<String, Vec<MemEntry>>, interval: Duration) {
    let mut dirty_since: Option<Instant> = None;
    loop {
        let received = match dirty_since {
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            Some(since) => rx.recv_timeout(interval.saturating_sub(since.elapsed())),
        };
        match received {
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

fn apply(entries: &mut HashMap<String, Vec<MemEntry>>, msg: Msg) {
    match msg {
        Msg::Upsert(ruby, entry) => {
            let list = entries.entry(ruby).or_default();
            match list.iter_mut().find(|m| m.word == entry.word && m.lcid == entry.lcid && m.rcid == entry.rcid) {
                Some(slot) => *slot = entry,
                None => list.push(entry),
            }
        }
        Msg::Prune(today) => {
            prune_entries(entries, today);
        }
        Msg::Remove(keys) => remove_keys(entries, &keys),
        Msg::Flush(_) => {}
    }
}

pub struct Learning {
    /// `None` なら無効 (読み書きしない)
    enabled: bool,
    /// 保存先。`None` なら保存しない (テスト用)
    path: Option<PathBuf>,
    entries: HashMap<String, Vec<MemEntry>>,
    len: usize,
    clock: Box<dyn Fn() -> u16>,
    cap: usize,
    /// 最後に `prune` した日。同じ日の確定では整理し直さない (整理は全件を舐めるので)。
    last_pruned: Option<u16>,
    /// 保存先があるときだけ。
    writer: Option<Writer>,
}

impl Learning {
    /// 読み書きしない学習。
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            path: None,
            entries: HashMap::new(),
            len: 0,
            clock: Box::new(system_today),
            cap: MAX_MEMORY_COUNT,
            last_pruned: None,
            writer: None,
        }
    }

    /// 環境変数 (`NOVAKEY_DISABLE_LEARNING=1` で無効、`NOVAKEY_MEMORY_DIR`) に従って開く。
    pub fn from_env() -> Self {
        if std::env::var("NOVAKEY_DISABLE_LEARNING").is_ok_and(|v| v == "1") {
            return Self::disabled();
        }
        match default_dir() {
            Some(dir) => Self::open(&dir),
            None => Self::disabled(),
        }
    }

    /// `dir` の `learning.tsv` を読んで開く。無いときは空、壊れているときは空にして
    /// 元のファイルを `learning.tsv.corrupt` に退避する (panic しない)。
    pub fn open(dir: &Path) -> Self {
        Self::open_with(dir, Box::new(system_today), FLUSH_INTERVAL)
    }

    /// 時計を差し替えて開く (テスト用)。
    #[cfg(test)]
    fn open_at(dir: &Path, clock: impl Fn() -> u16 + 'static) -> Self {
        Self::open_with(dir, Box::new(clock), FLUSH_INTERVAL)
    }

    /// 読み込んだら、古いものの整理と上限の適用を先に済ませる (確定のたびではなく、ここと日付が変わったとき)。
    fn open_with(dir: &Path, clock: Box<dyn Fn() -> u16>, interval: Duration) -> Self {
        let path = dir.join(FILE_NAME);
        let mut me = Self { enabled: true, path: Some(path.clone()), clock, ..Self::disabled() };
        me.load();
        let today = (me.clock)();
        prune_entries(&mut me.entries, today);
        me.last_pruned = Some(today);
        me.len = me.entries.values().map(Vec::len).sum();
        me.enforce_cap(&HashSet::new());
        me.writer = Writer::spawn(path, me.entries.clone(), interval);
        if me.writer.is_none() {
            crate::platform::log("learning writer unavailable; learning stays in memory");
        }
        me
    }

    /// 未保存の変更を書き終えるまで待つ (テスト用。通常は背景スレッドが間隔を空けて書く)。
    #[cfg(test)]
    pub(crate) fn flush(&self) {
        let Some(writer) = &self.writer else { return };
        let (tx, rx) = mpsc::channel();
        writer.send(Msg::Flush(tx));
        let _ = rx.recv_timeout(Duration::from_secs(10));
    }

    /// 保存しない有効な学習 (単体テスト用)。
    pub fn in_memory() -> Self {
        Self { enabled: true, ..Self::disabled() }
    }

    pub fn with_clock(mut self, clock: impl Fn() -> u16 + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    pub fn with_cap(mut self, cap: usize) -> Self {
        self.cap = cap;
        self
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn entries_for(&self, ruby: &str) -> &[MemEntry] {
        self.entries.get(ruby).map_or(&[], Vec::as_slice)
    }

    fn load(&mut self) {
        let Some(path) = self.path.clone() else { return };
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                eprintln!("novakey_r: 学習データを読めませんでした ({}): {e}", path.display());
                return;
            }
        };
        match parse(&bytes) {
            Some(parsed) => {
                self.len = parsed.entries.values().map(Vec::len).sum();
                self.entries = parsed.entries;
                if parsed.skipped > 0 {
                    // 次の保存で壊れた行が消えるので、元のファイルは写しを残す。
                    crate::platform::log(&format!("learning file: skipped {} unreadable line(s)", parsed.skipped));
                    if let Err(e) = backup(&path, false) {
                        eprintln!("novakey_r: 学習データの退避に失敗しました: {e}");
                    }
                }
            }
            None => {
                crate::platform::log("learning file unreadable; starting empty");
                if let Err(e) = backup(&path, true) {
                    eprintln!("novakey_r: 学習データの退避に失敗しました: {e}");
                }
            }
        }
    }

    /// `katakana` の先頭から始まる、読みが prefix になっている学習エントリ (短い順)。
    pub fn lookup(&self, katakana: &str, out: &mut Vec<Found>) {
        if !self.enabled || self.len == 0 {
            return;
        }
        let mut chars = 0;
        for (idx, c) in katakana.char_indices() {
            chars += 1;
            let ruby = &katakana[..idx + c.len_utf8()];
            let Some(list) = self.entries.get(ruby) else { continue };
            let ruby_len = count_graphemes(ruby);
            for m in list {
                let elem = Elem::new(m.word.clone(), ruby.to_string(), m.lcid, m.rcid, m.mid, value_for(m.count, ruby_len));
                out.push(Found { elem: Rc::new(elem), ruby_chars: chars });
            }
        }
    }

    /// 確定した候補 (`Candidate.data`) を覚えて保存する (`update` + `commitUpdateLearningData`)。
    /// `known` は辞書の文字表にある文字か (無い文字を含む読みは覚えない)。
    pub fn learn(&mut self, data: &[Rc<Elem>], known: &dyn Fn(char) -> bool) {
        if !self.enabled || data.is_empty() {
            return;
        }
        let today = (self.clock)();
        let mut touched: HashSet<EntryKey> = HashSet::new();
        // 保存済みのものを先に整理する (merge の「古いデータの選別」)。日付が変わったときだけ。
        if self.last_pruned != Some(today) {
            self.prune(today);
        }
        let mut memorize = |me: &mut Self, p: &Piece| {
            if p.ruby.is_empty() || !p.ruby.chars().all(known) {
                return;
            }
            me.memorize(p, today);
            touched.insert((p.ruby.clone(), p.word.clone(), p.lcid, p.rcid));
        };

        // 単語単位
        for e in data {
            if need_w_value_memory(e.lcid) {
                memorize(self, &Piece::of(e));
            }
        }
        if data.len() > 1 {
            // 文節単位 bigram (第 1 文節 + 第 2 文節)
            let mut first: Option<Piece> = None;
            let mut second: Option<Piece> = None;
            for e in data {
                let Some(f) = first.as_mut() else {
                    first = Some(Piece::of(e));
                    continue;
                };
                let clause = is_clause(f.rcid, e.lcid);
                match second.as_mut() {
                    None if clause => second = Some(Piece::of(e)),
                    None => f.append(e),
                    Some(s) if clause => {
                        let element = Piece::compound(f, s);
                        first = second.take();
                        second = Some(Piece::of(e));
                        memorize(self, &element);
                    }
                    Some(s) => s.append(e),
                }
            }
            if let (Some(f), Some(s)) = (&first, &second) {
                memorize(self, &Piece::compound(f, s));
            }
            // 全体
            let whole = Piece {
                word: data.iter().map(|e| e.word.as_str()).collect(),
                ruby: data.iter().map(|e| e.ruby.as_str()).collect(),
                lcid: data[0].lcid,
                rcid: data[data.len() - 1].rcid,
                mid: data[data.len() - 1].mid,
                base: data.iter().fold(0.0, |acc, e| acc + e.base),
            };
            memorize(self, &whole);
        }
        self.send_upserts(&touched);
        let removed = self.enforce_cap(&touched);
        if !removed.is_empty() {
            self.emit(Msg::Remove(removed));
        }
    }

    fn emit(&self, msg: Msg) {
        if let Some(writer) = &self.writer {
            writer.send(msg);
        }
    }

    /// 今回触れたエントリの最終状態を写しに流す。
    fn send_upserts(&self, touched: &HashSet<EntryKey>) {
        if self.writer.is_none() {
            return;
        }
        for (ruby, word, lcid, rcid) in touched {
            let found = self
                .entries
                .get(ruby)
                .and_then(|list| list.iter().find(|m| m.word == *word && m.lcid == *lcid && m.rcid == *rcid));
            if let Some(m) = found {
                self.emit(Msg::Upsert(ruby.clone(), m.clone()));
            }
        }
    }

    /// 1 件を覚える。同じ (読み, 表層, lcid, rcid) があれば出現数を足す。
    fn memorize(&mut self, p: &Piece, today: u16) {
        let list = self.entries.entry(p.ruby.clone()).or_default();
        if let Some(m) = list.iter_mut().find(|m| m.word == p.word && m.lcid == p.lcid && m.rcid == p.rcid) {
            m.count = m.count.saturating_add(1);
            m.last_used = today;
            m.last_updated = today;
        } else {
            list.push(MemEntry { word: p.word.clone(), lcid: p.lcid, rcid: p.rcid, mid: p.mid, count: 1, last_used: today, last_updated: today });
            self.len += 1;
        }
    }

    /// 未来の日付のリセット・128 日以上使っていないものの削除・32 日ごとの半減。
    pub fn prune(&mut self, today: u16) {
        self.len -= prune_entries(&mut self.entries, today);
        self.last_pruned = Some(today);
        self.emit(Msg::Prune(today));
    }

    /// 上限を超えた分を、読みの長いものから捨てる (今回触れたものは残す)。捨てたキーを返す。
    /// 全件を並べ替えるので、上限ちょうどからは毎回ではなく、上限の 1/16 を超えたときにまとめて上限まで減らす。
    fn enforce_cap(&mut self, touched: &HashSet<EntryKey>) -> Vec<EntryKey> {
        if self.len <= self.cap + self.cap / 16 {
            return Vec::new();
        }
        let mut keys: Vec<(usize, String, String, usize, usize)> = self
            .entries
            .iter()
            .flat_map(|(ruby, list)| list.iter().map(move |m| (count_graphemes(ruby), ruby.clone(), m.word.clone(), m.lcid, m.rcid)))
            .filter(|(_, ruby, word, l, r)| !touched.contains(&(ruby.clone(), word.clone(), *l, *r)))
            .collect();
        // 長い順 (同じ長さなら読み・表層の降順) に並べて先頭から捨てる
        keys.sort_by(|a, b| b.cmp(a));
        let excess = self.len - self.cap;
        let removed: Vec<EntryKey> = keys.into_iter().take(excess).map(|(_, ruby, word, l, r)| (ruby, word, l, r)).collect();
        remove_keys(&mut self.entries, &removed);
        self.len = self.entries.values().map(Vec::len).sum();
        removed
    }
}

/// 整理して、消した件数を返す。
fn prune_entries(entries: &mut HashMap<String, Vec<MemEntry>>, today: u16) -> usize {
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

fn remove_keys(entries: &mut HashMap<String, Vec<MemEntry>>, keys: &[EntryKey]) {
    for (ruby, word, lcid, rcid) in keys {
        if let Some(list) = entries.get_mut(ruby) {
            list.retain(|m| !(m.word == *word && m.lcid == *lcid && m.rcid == *rcid));
            if list.is_empty() {
                entries.remove(ruby);
            }
        }
    }
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// `learning.tsv` に書く。pid と通し番号で一意にした一時ファイルへ書き、`sync_all` してから rename する
/// (電源断でも、半端なファイルが本物に置き換わらない)。背景スレッドから呼ぶ。
/// IME プロセスが同時に複数いると後から書いたものが勝つ (last-writer-wins) が、通常は 1 つなので許容する。
fn write_file(path: &Path, entries: &HashMap<String, Vec<MemEntry>>) {
    let result = (|| -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = path.with_extension(format!("tsv.tmp-{}-{n}", std::process::id()));
        let written = (|| -> std::io::Result<()> {
            let mut file = std::fs::File::create(&tmp)?;
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
        eprintln!("novakey_r: 学習データを保存できませんでした ({}): {e}", path.display());
    }
}

/// 壊れたファイルを `<name>.corrupt-<unix 秒>` に残す。同名があれば `-1`, `-2` ... を付けて、前の退避は上書きしない。
/// `rename` が false なら写しを取る (元のファイルは次の保存で置き換わる)。
fn backup(path: &Path, rename: bool) -> std::io::Result<()> {
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
        std::fs::copy(path, target).map(|_| ())
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
fn serialize(entries: &HashMap<String, Vec<MemEntry>>) -> String {
    let mut rubies: Vec<&String> = entries.keys().collect();
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

struct Parsed {
    entries: HashMap<String, Vec<MemEntry>>,
    /// 読めずに飛ばした行数 (空行は数えない)
    skipped: usize,
}

/// 1 行ずつ読み、読めない行・重複する行は飛ばして数える。ヘッダが違う (別のファイル・壊れている) ときだけ `None`。
fn parse(bytes: &[u8]) -> Option<Parsed> {
    let mut lines = bytes.split(|&b| b == b'\n');
    let header = std::str::from_utf8(lines.next()?).ok()?;
    if header.trim_end_matches('\r') != HEADER {
        return None;
    }
    let mut entries: HashMap<String, Vec<MemEntry>> = HashMap::new();
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
        if list.iter().any(|x| x.word == m.word && x.lcid == m.lcid && x.rcid == m.rcid) {
            skipped += 1;
            continue;
        }
        list.push(m);
    }
    Some(Parsed { entries, skipped })
}

fn parse_line(line: &str) -> Option<(String, MemEntry)> {
    let f: Vec<&str> = line.split('\t').collect();
    if f.len() != 8 {
        return None;
    }
    let ruby = unescape(f[0])?;
    if ruby.is_empty() {
        return None;
    }
    let m = MemEntry {
        word: unescape(f[1])?,
        lcid: f[2].parse().ok()?,
        rcid: f[3].parse().ok()?,
        mid: f[4].parse().ok()?,
        count: f[5].parse().ok()?,
        last_used: f[6].parse().ok()?,
        last_updated: f[7].parse().ok()?,
    };
    Some((ruby, m))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn el(word: &str, ruby: &str, lcid: usize, rcid: usize, mid: usize, base: f32) -> Rc<Elem> {
        Rc::new(Elem::new(word.into(), ruby.into(), lcid, rcid, mid, base))
    }

    fn known(_: char) -> bool {
        true
    }

    fn clock(day: &Rc<Cell<u16>>) -> impl Fn() -> u16 + 'static {
        let d = day.clone();
        move || d.get()
    }

    fn found(l: &Learning, ruby: &str) -> Vec<(String, f32)> {
        let mut out = Vec::new();
        l.lookup(ruby, &mut out);
        out.iter().map(|f| (f.elem.word.clone(), f.elem.base)).collect()
    }

    fn tmp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("novakey-r-learning-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn value_formula_matches_swift_ranges() {
        // 長さ 1 のとき [-5, -8]、2 のとき [-3, -6]、4 のとき [-2, -5]
        assert!((value_for(1, 1) - (-1.0 - 4.0 - 3.0 * (254.0f64 / 255.0).powi(3)) as f32).abs() < 1e-6);
        assert!((value_for(255, 2) - -3.0).abs() < 1e-6);
        assert!((value_for(255, 4) - -2.0).abs() < 1e-6);
    }

    #[test]
    fn word_level_memory_follows_need_w_value_memory() {
        assert!(need_w_value_memory(1288));
        assert!(!need_w_value_memory(200)); // 助詞・助動詞
        assert!(!need_w_value_memory(0));
        assert!(!need_w_value_memory(1316));
        assert!(!need_w_value_memory(1300)); // 接尾名詞
        assert!(!need_w_value_memory(7)); // 記号
    }

    #[test]
    fn memorized_word_is_found_by_every_prefix_length() {
        let mut l = Learning::in_memory();
        l.learn(&[el("幹事", "カンジ", 1288, 1288, 501, -7.0)], &known);
        assert_eq!(l.len(), 1);
        assert!(found(&l, "カ").is_empty());
        assert_eq!(found(&l, "カンジ").len(), 1);
        assert_eq!(found(&l, "カンジヲミル").len(), 1);
        let v = l.entries_for("カンジ")[0].count;
        assert_eq!(v, 1);
        let mut out = Vec::new();
        l.lookup("カンジ", &mut out);
        assert_eq!(out[0].ruby_chars, 3);
        assert!((out[0].elem.base - value_for(1, 3)).abs() < 1e-6);
    }

    #[test]
    fn repeated_commits_accumulate_count_up_to_255() {
        let mut l = Learning::in_memory();
        let d = [el("幹事", "カンジ", 1288, 1288, 501, -7.0)];
        for _ in 0..300 {
            l.learn(&d, &known);
        }
        assert_eq!(l.entries_for("カンジ")[0].count, 255);
        assert_eq!(l.len(), 1);
    }

    #[test]
    fn particles_alone_are_not_memorized_but_compounds_are() {
        let mut l = Learning::in_memory();
        // 名詞 + 助詞: 単語は名詞だけ、複合 (第 1 + 第 2 文節) は文節が 1 つなので全体だけ
        let data = [el("今日", "キョウ", 1288, 1288, 501, -6.0), el("は", "ハ", 200, 200, 10, -3.0)];
        l.learn(&data, &known);
        assert_eq!(l.entries_for("キョウ").len(), 1);
        assert!(l.entries_for("ハ").is_empty());
        assert_eq!(l.entries_for("キョウハ").len(), 1);
        let whole = &l.entries_for("キョウハ")[0];
        assert_eq!((whole.word.as_str(), whole.lcid, whole.rcid, whole.mid), ("今日は", 1288, 200, 10));
    }

    #[test]
    fn two_clause_candidate_memorizes_the_compound_and_the_whole() {
        let mut l = Learning::in_memory();
        // 名詞 助詞 | 動詞 (内容語どうしは文節が切れる)
        let data = [
            el("本", "ホン", 1288, 1288, 501, -6.0),
            el("を", "ヲ", 200, 200, 10, -3.0),
            el("読む", "ヨム", 700, 700, 400, -6.0),
        ];
        let boundaries = (is_clause(data[0].rcid, data[1].lcid), is_clause(data[1].rcid, data[2].lcid));
        assert_eq!(boundaries, (false, true));
        l.learn(&data, &known);
        // 単語: 本 / 読む。複合 (本を + 読む) と全体は表層・読みが同じでも rcid が違う (助詞 / 動詞) ので別のエントリ
        assert_eq!(l.entries_for("ホン").len(), 1);
        assert_eq!(l.entries_for("ヨム").len(), 1);
        let both = l.entries_for("ホンヲヨム");
        assert_eq!(both.len(), 2);
        assert!(both.iter().all(|m| m.word == "本を読む" && m.count == 1));
        let mut rcids: Vec<usize> = both.iter().map(|m| m.rcid).collect();
        rcids.sort();
        assert_eq!(rcids, [200, 700]);
    }

    #[test]
    fn unknown_chars_are_skipped() {
        let mut l = Learning::in_memory();
        l.learn(&[el("幹事", "カンジ", 1288, 1288, 501, -7.0)], &|c| c != 'ン');
        assert!(l.is_empty());
    }

    #[test]
    fn halves_count_every_32_days_and_expires_after_128() {
        let day = Rc::new(Cell::new(1000u16));
        let mut l = Learning::in_memory().with_clock(clock(&day));
        let d = [el("幹事", "カンジ", 1288, 1288, 501, -7.0)];
        for _ in 0..8 {
            l.learn(&d, &known);
        }
        assert_eq!(l.entries_for("カンジ")[0].count, 8);
        // 32 日ちょうどでは半減しない
        l.prune(1032);
        assert_eq!(l.entries_for("カンジ")[0].count, 8);
        // 33 日で 1 回、65 日で 2 回
        l.prune(1033);
        assert_eq!(l.entries_for("カンジ")[0].count, 4);
        l.prune(1033);
        assert_eq!(l.entries_for("カンジ")[0].count, 4, "同じ日に何度整理しても半減は 1 回");
        l.prune(1065);
        assert_eq!(l.entries_for("カンジ")[0].count, 2);
        // 128 日未使用で消える
        l.prune(1127);
        assert_eq!(l.len(), 1);
        l.prune(1128);
        assert!(l.is_empty());
    }

    #[test]
    fn count_that_halves_to_zero_is_dropped() {
        let day = Rc::new(Cell::new(1000u16));
        let mut l = Learning::in_memory().with_clock(clock(&day));
        l.learn(&[el("幹事", "カンジ", 1288, 1288, 501, -7.0)], &known);
        l.prune(1033);
        assert!(l.is_empty());
    }

    #[test]
    fn use_after_halving_keeps_the_entry_fresh() {
        let day = Rc::new(Cell::new(1000u16));
        let mut l = Learning::in_memory().with_clock(clock(&day));
        let d = [el("幹事", "カンジ", 1288, 1288, 501, -7.0)];
        for _ in 0..4 {
            l.learn(&d, &known);
        }
        day.set(1100);
        l.learn(&d, &known); // 先に整理 (100 日 → 3 回半減で 0 ...) してから覚えるので 1 から数え直し
        assert_eq!(l.entries_for("カンジ")[0].count, 1);
        assert_eq!(l.entries_for("カンジ")[0].last_used, 1100);
    }

    #[test]
    fn future_dates_are_reset() {
        let day = Rc::new(Cell::new(1000u16));
        let mut l = Learning::in_memory().with_clock(clock(&day));
        let d = [el("幹事", "カンジ", 1288, 1288, 501, -7.0)];
        for _ in 0..5 {
            l.learn(&d, &known);
        }
        day.set(900); // 時計が戻った
        l.prune(900);
        let m = &l.entries_for("カンジ")[0];
        assert_eq!((m.count, m.last_used, m.last_updated), (1, 900, 900));
    }

    #[test]
    fn cap_drops_the_longest_readings_but_keeps_what_was_just_learned() {
        let mut l = Learning::in_memory().with_cap(3);
        l.learn(&[el("あ", "ア", 1288, 1288, 501, -7.0)], &known);
        l.learn(&[el("いい", "イイ", 1288, 1288, 501, -7.0)], &known);
        l.learn(&[el("ううう", "ウウウ", 1288, 1288, 501, -7.0)], &known);
        assert_eq!(l.len(), 3);
        l.learn(&[el("えええ", "エエエ", 1288, 1288, 501, -7.0)], &known);
        assert_eq!(l.len(), 3);
        assert!(l.entries_for("エエエ").len() == 1, "新しく覚えたものは残る");
        assert!(l.entries_for("ウウウ").is_empty(), "最も長い古いものが捨てられる");
        assert!(!l.entries_for("ア").is_empty() && !l.entries_for("イイ").is_empty());
    }

    #[test]
    fn persists_and_reloads() {
        let dir = tmp_dir("roundtrip");
        let day = Rc::new(Cell::new(1000u16));
        {
            let mut l = Learning::open_at(&dir, clock(&day));
            l.learn(&[el("幹\t事", "カ\\ンジ", 1288, 1288, 501, -7.0)], &known);
            l.learn(&[el("幹事", "カンジ", 1288, 1288, 501, -7.0)], &known);
            l.learn(&[el("幹事", "カンジ", 1288, 1288, 501, -7.0)], &known);
        }
        assert!(dir.join("learning.tsv").exists());
        assert!(!dir.join("learning.tsv.tmp").exists(), "一時ファイルは rename で消える");
        let l = Learning::open_at(&dir, clock(&day));
        assert_eq!(l.len(), 2);
        assert_eq!(l.entries_for("カンジ")[0].count, 2);
        assert_eq!(l.entries_for("カ\\ンジ")[0].word, "幹\t事");
        // 同じ内容なら同じバイト列
        assert_eq!(serialize(&l.entries), std::fs::read_to_string(dir.join("learning.tsv")).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disabled_reads_and_writes_nothing() {
        let dir = tmp_dir("disabled");
        let mut l = Learning::disabled();
        l.learn(&[el("幹事", "カンジ", 1288, 1288, 501, -7.0)], &known);
        assert!(l.is_empty());
        let mut out = Vec::new();
        l.lookup("カンジ", &mut out);
        assert!(out.is_empty());
        assert!(!dir.exists());
    }

    fn backups(dir: &Path) -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().contains(".corrupt-")))
            .collect();
        found.sort();
        found
    }

    #[test]
    fn unreadable_file_is_backed_up_and_treated_as_empty() {
        for (name, content) in [
            ("garbage", b"\xff\xfe\x00 not a tsv".to_vec()),
            ("header", "# other\nカンジ\t幹事\t1\t1\t1\t1\t1\t1\n".as_bytes().to_vec()),
        ] {
            let dir = tmp_dir(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("learning.tsv"), &content).unwrap();
            let mut l = Learning::open(&dir);
            assert!(l.is_empty(), "{name}");
            let saved = backups(&dir);
            assert_eq!(saved.len(), 1, "{name}: 退避される");
            assert_eq!(std::fs::read(&saved[0]).unwrap(), content, "{name}: 中身はそのまま");
            assert!(!dir.join("learning.tsv").exists(), "{name}");
            // その後も普通に学習できる
            l.learn(&[el("幹事", "カンジ", 1288, 1288, 501, -7.0)], &known);
            drop(l);
            assert_eq!(Learning::open(&dir).len(), 1, "{name}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn bad_lines_are_skipped_without_losing_the_good_ones() {
        let good = "ゴ\t語\t1288\t1288\t501\t2\t1700\t1700";
        for (name, bad) in [
            ("fields", "カンジ\t幹事\t1".to_string()),
            ("number", "カンジ\t幹事\tx\t1\t1\t1\t1\t1".to_string()),
            ("range", "カンジ\t幹事\t1\t1\t1\t999\t1\t1".to_string()),
            ("escape", "カンジ\t幹\\q\t1\t1\t1\t1\t1\t1".to_string()),
            ("dup", good.to_string()),
            ("empty ruby", "\t幹事\t1\t1\t1\t1\t1\t1".to_string()),
        ] {
            let dir = tmp_dir(&format!("skip-{}", name.replace(' ', "-")));
            std::fs::create_dir_all(&dir).unwrap();
            let original = format!("{HEADER}\n{good}\n{bad}\n");
            std::fs::write(dir.join("learning.tsv"), &original).unwrap();
            let day = Rc::new(Cell::new(1700u16));
            let l = Learning::open_at(&dir, clock(&day));
            assert_eq!(l.len(), 1, "{name}");
            assert_eq!(l.entries_for("ゴ")[0].count, 2, "{name}");
            let saved = backups(&dir);
            assert_eq!(saved.len(), 1, "{name}: 壊れた行があれば元のファイルの写しを残す");
            assert_eq!(std::fs::read_to_string(&saved[0]).unwrap(), original, "{name}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn earlier_backups_are_never_overwritten() {
        let dir = tmp_dir("backups");
        std::fs::create_dir_all(&dir).unwrap();
        for round in 0..3 {
            std::fs::write(dir.join("learning.tsv"), format!("broken {round}")).unwrap();
            drop(Learning::open(&dir));
        }
        let saved = backups(&dir);
        assert_eq!(saved.len(), 3);
        let mut bodies: Vec<String> = saved.iter().map(|p| std::fs::read_to_string(p).unwrap()).collect();
        bodies.sort();
        assert_eq!(bodies, ["broken 0", "broken 1", "broken 2"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn temp_files_have_unique_names_and_are_synced_then_renamed() {
        let dir = tmp_dir("tmpnames");
        let mut entries = HashMap::new();
        entries.insert("ア".to_string(), vec![MemEntry { word: "亜".into(), lcid: 1, rcid: 1, mid: 1, count: 1, last_used: 1, last_updated: 1 }]);
        let path = dir.join("learning.tsv");
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| (0..10).for_each(|_| write_file(&path, &entries)));
            }
        });
        assert_eq!(std::fs::read_to_string(&path).unwrap(), serialize(&entries));
        let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).filter(|e| e.file_name() != "learning.tsv").collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_is_empty() {
        let dir = tmp_dir("missing");
        assert!(Learning::open(&dir).is_empty());
        assert!(!dir.exists(), "読むだけでは作らない");
    }

    #[test]
    fn writes_are_coalesced_and_the_file_matches_memory() {
        let dir = tmp_dir("coalesce");
        let day = Rc::new(Cell::new(1000u16));
        let mut l = Learning::open_with(&dir, Box::new(clock(&day)), Duration::from_secs(3600));
        for i in 0..20 {
            l.learn(&[el(&format!("語{i}"), &format!("ゴ{i}"), 1288, 1288, 501, -7.0)], &known);
        }
        assert!(!dir.join("learning.tsv").exists(), "間隔内では書かない");
        l.flush();
        assert_eq!(serialize(&l.entries), std::fs::read_to_string(dir.join("learning.tsv")).unwrap());
        // 日付が変わった整理も写しに届く
        day.set(1100);
        l.learn(&[el("新", "シン", 1288, 1288, 501, -7.0)], &known);
        l.flush();
        assert_eq!(l.len(), 1);
        assert_eq!(serialize(&l.entries), std::fs::read_to_string(dir.join("learning.tsv")).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_file_follows_cap_evictions() {
        let dir = tmp_dir("cap-mirror");
        let day = Rc::new(Cell::new(1000u16));
        let mut l = Learning::open_with(&dir, Box::new(clock(&day)), Duration::from_secs(3600)).with_cap(3);
        for ruby in ["ア", "イイ", "ウウウ", "エエエエ", "オ"] {
            l.learn(&[el(ruby, ruby, 1288, 1288, 501, -7.0)], &known);
        }
        l.flush();
        assert_eq!(l.len(), 3);
        assert_eq!(serialize(&l.entries), std::fs::read_to_string(dir.join("learning.tsv")).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dropping_flushes_pending_changes() {
        let dir = tmp_dir("drop-flush");
        let mut l = Learning::open_with(&dir, Box::new(system_today), Duration::from_secs(3600));
        l.learn(&[el("幹事", "カンジ", 1288, 1288, 501, -7.0)], &known);
        drop(l);
        assert_eq!(Learning::open(&dir).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_interval_timer_writes_without_a_flush() {
        let dir = tmp_dir("timer");
        let mut l = Learning::open_with(&dir, Box::new(system_today), Duration::from_millis(50));
        l.learn(&[el("幹事", "カンジ", 1288, 1288, 501, -7.0)], &known);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !dir.join("learning.tsv").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(dir.join("learning.tsv").exists());
        drop(l);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 手元計測用 (`LEARNING_BENCH_DIR=<learning.tsv のあるディレクトリ> cargo test --release bench_commit_cost -- --ignored --nocapture`)。
    /// 読み込み済みの大きな記憶に 1 件ずつ確定したときの、メインスレッドの 1 回あたりの時間を出す。
    #[test]
    #[ignore]
    fn bench_commit_cost() {
        let Some(src) = std::env::var_os("LEARNING_BENCH_DIR") else { return };
        let dir = tmp_dir("bench");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(Path::new(&src).join("learning.tsv"), dir.join("learning.tsv")).unwrap();
        let mut l = Learning::open(&dir);
        println!("entries={}", l.len());
        let mut times = Vec::new();
        for i in 0..40 {
            let d = [el(&format!("新語{i}"), &format!("シンゴ{i}"), 1288, 1288, 501, -7.0)];
            let t = std::time::Instant::now();
            l.learn(&d, &known);
            times.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        times.sort_by(|a, b| a.total_cmp(b));
        println!("learn() ms: min={:.2} median={:.2} p90={:.2} max={:.2}", times[0], times[times.len() / 2], times[times.len() * 9 / 10], times[times.len() - 1]);
        drop(l);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
