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

mod format;
mod store;
mod writer;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
#[cfg(test)]
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::elem::{count_graphemes, include_mm_value_calculation, is_clause, Elem};
use super::lattice::Found;
#[cfg(test)]
use format::{serialize, write_file, HEADER};
use format::{backup, parse, FILE_NAME};
use store::{count_entries, pick_evictions, prune_entries, remove_keys, Entries, EntryKey, Text};
use writer::{Msg, Writer};

pub use store::MemEntry;


/// 覚えるエントリ数の上限 (`maxMemoryCount`)
pub const MAX_MEMORY_COUNT: usize = 65536;
const DIR_NAME: &str = "rust-learning";
/// 最初の未保存の変更から、この時間だけ待ってまとめて書く。
const FLUSH_INTERVAL: Duration = Duration::from_millis(1500);


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

/// 学習データの置き場所 (`<設定の memory_dir>/rust-learning`)。
pub fn default_dir(config: &crate::config::Config) -> Option<PathBuf> {
    config.memory_dir.as_ref().map(|base| base.join(DIR_NAME))
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

pub struct Learning {
    /// `None` なら無効 (読み書きしない)
    enabled: bool,
    /// 保存先。`None` なら保存しない (テスト用)
    path: Option<PathBuf>,
    entries: Entries,
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
            entries: Entries::new(),
            len: 0,
            clock: Box::new(system_today),
            cap: MAX_MEMORY_COUNT,
            last_pruned: None,
            writer: None,
        }
    }

    /// 設定 (学習の有効・記憶先) に従って開く。
    pub fn from_config(config: &crate::config::Config) -> Self {
        if !config.learning_enabled {
            return Self::disabled();
        }
        match default_dir(config) {
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
        me.len = count_entries(&me.entries);
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

    #[cfg(test)]
    /// 保存しない有効な学習 (単体テスト用)。
    pub fn in_memory() -> Self {
        Self { enabled: true, ..Self::disabled() }
    }

    #[cfg(test)]
    pub fn with_clock(mut self, clock: impl Fn() -> u16 + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    #[cfg(test)]
    pub fn with_cap(mut self, cap: usize) -> Self {
        self.cap = cap;
        self
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.len
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[cfg(test)]
    pub fn entries_for(&self, ruby: &str) -> &[MemEntry] {
        self.entries.get(ruby).map_or(&[], Vec::as_slice)
    }

    fn load(&mut self) {
        let Some(path) = self.path.clone() else { return };
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                crate::platform::log(&format!("learning file not readable: {e}"));
                return;
            }
        };
        match parse(&bytes) {
            Some(parsed) => {
                self.len = count_entries(&parsed.entries);
                self.entries = parsed.entries;
                if parsed.skipped > 0 {
                    // 次の保存で壊れた行が消えるので、元のファイルは写しを残す。
                    crate::platform::log(&format!("learning file: skipped {} unreadable line(s)", parsed.skipped));
                    if let Err(e) = backup(&path, false) {
                        crate::platform::log(&format!("learning file backup failed: {e}"));
                    }
                }
            }
            None => {
                crate::platform::log("learning file unreadable; starting empty");
                if let Err(e) = backup(&path, true) {
                    crate::platform::log(&format!("learning file backup failed: {e}"));
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
                let elem = Elem::new(m.word.to_string(), ruby.to_string(), m.lcid, m.rcid, m.mid, value_for(m.count, ruby_len));
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
            touched.insert(me.memorize(p, today));
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
            let Some((stored, list)) = self.entries.get_key_value(ruby) else { continue };
            if let Some(m) = list.iter().find(|m| m.is_key(word, *lcid, *rcid)) {
                self.emit(Msg::Upsert(stored.clone(), m.clone()));
            }
        }
    }

    /// 1 件を覚える。同じ (読み, 表層, lcid, rcid) があれば出現数を足す。覚えたエントリのキーを返す。
    fn memorize(&mut self, p: &Piece, today: u16) -> EntryKey {
        let ruby = Text::from(p.ruby.as_str());
        let list = self.entries.entry(ruby.clone()).or_default();
        let word = match list.iter_mut().find(|m| m.is_key(&p.word, p.lcid, p.rcid)) {
            Some(m) => {
                m.count = m.count.saturating_add(1);
                m.last_used = today;
                m.last_updated = today;
                m.word.clone()
            }
            None => {
                let word = Text::from(p.word.as_str());
                list.push(MemEntry { word: word.clone(), lcid: p.lcid, rcid: p.rcid, mid: p.mid, count: 1, last_used: today, last_updated: today });
                self.len += 1;
                word
            }
        };
        (ruby, word, p.lcid, p.rcid)
    }

    /// 未来の日付のリセット・128 日以上使っていないものの削除・32 日ごとの半減。
    pub fn prune(&mut self, today: u16) {
        self.len -= prune_entries(&mut self.entries, today);
        self.last_pruned = Some(today);
        self.emit(Msg::Prune(today));
    }

    /// 上限を超えた分を、読みの長いものから捨てる (今回触れたものは残す)。捨てたキーを返す。
    /// 捨てるものを選ぶのに全件を舐めるので、上限ちょうどからは毎回ではなく、上限の 1/16 を超えたときにまとめて上限まで減らす。
    fn enforce_cap(&mut self, touched: &HashSet<EntryKey>) -> Vec<EntryKey> {
        if self.len <= self.cap + self.cap / 16 {
            return Vec::new();
        }
        let removed = pick_evictions(&self.entries, touched, self.len - self.cap);
        remove_keys(&mut self.entries, &removed);
        self.len = count_entries(&self.entries);
        removed
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::time::Instant;

    use super::*;

    fn el(word: &str, ruby: &str, lcid: usize, rcid: usize, mid: usize, base: f32) -> Rc<Elem> {
        Rc::new(Elem::new(word.into(), ruby.into(), lcid, rcid, mid, base))
    }

    fn known(_: char) -> bool {
        true
    }

    fn clock(day: &Rc<Cell<u16>>) -> impl Fn() -> u16 + 'static + use<> {
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
    fn write_file_replaces_a_world_readable_file_with_a_0600_one() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_dir("perm");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("learning.tsv");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_file(&path, &Entries::new());
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&dir), 0o700);
        let _ = std::fs::remove_dir_all(&dir);
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
        let mut entries = Entries::new();
        entries.insert("ア".into(), vec![MemEntry { word: "亜".into(), lcid: 1, rcid: 1, mid: 1, count: 1, last_used: 1, last_updated: 1 }]);
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
