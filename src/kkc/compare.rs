//! Swift 版の golden (tools/golden/*.jsonl.gz) と Rust 版の変換結果を比べる差分ツール (`--kkc-compare`)。
//!
//! golden の読み込みは `gzip -dc` を外部コマンドで呼び、JSON は下の小さなパーサで読む (依存を増やさない)。
//! 同点 (value の f32 bit が等しい連続区間) の入れ替わりは差としない。
#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use regex::Regex;

use super::result::{Cand, Converted};
use super::KkcCore;

// ---------------------------------------------------------------- JSON

#[derive(Debug, Clone)]
pub enum Json {
    Null,
    /// golden に真偽値は出てこないので値は持たない
    Bool,
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(kv) => kv.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    fn str(&self) -> &str {
        match self {
            Json::Str(s) => s,
            _ => "",
        }
    }
    fn num(&self) -> f64 {
        match self {
            Json::Num(n) => *n,
            _ => 0.0,
        }
    }
    fn arr(&self) -> &[Json] {
        match self {
            Json::Arr(a) => a,
            _ => &[],
        }
    }
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\n' | b'\r' | b'\t') {
            self.i += 1;
        }
    }

    fn expect(&mut self, c: u8) -> Result<(), String> {
        if self.b.get(self.i) == Some(&c) {
            self.i += 1;
            Ok(())
        } else {
            Err(format!("'{}' が必要です (位置 {})", c as char, self.i))
        }
    }

    fn value(&mut self) -> Result<Json, String> {
        self.ws();
        match self.b.get(self.i).copied() {
            Some(b'{') => {
                self.i += 1;
                let mut kv = Vec::new();
                self.ws();
                if self.b.get(self.i) == Some(&b'}') {
                    self.i += 1;
                    return Ok(Json::Obj(kv));
                }
                loop {
                    self.ws();
                    let k = self.string()?;
                    self.ws();
                    self.expect(b':')?;
                    let v = self.value()?;
                    kv.push((k, v));
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(Json::Obj(kv));
                        }
                        _ => return Err(format!("オブジェクトが閉じていません (位置 {})", self.i)),
                    }
                }
            }
            Some(b'[') => {
                self.i += 1;
                let mut a = Vec::new();
                self.ws();
                if self.b.get(self.i) == Some(&b']') {
                    self.i += 1;
                    return Ok(Json::Arr(a));
                }
                loop {
                    a.push(self.value()?);
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(Json::Arr(a));
                        }
                        _ => return Err(format!("配列が閉じていません (位置 {})", self.i)),
                    }
                }
            }
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') if self.b[self.i..].starts_with(b"true") => {
                self.i += 4;
                Ok(Json::Bool)
            }
            Some(b'f') if self.b[self.i..].starts_with(b"false") => {
                self.i += 5;
                Ok(Json::Bool)
            }
            Some(b'n') if self.b[self.i..].starts_with(b"null") => {
                self.i += 4;
                Ok(Json::Null)
            }
            Some(_) => {
                let start = self.i;
                while self.i < self.b.len() && matches!(self.b[self.i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') {
                    self.i += 1;
                }
                std::str::from_utf8(&self.b[start..self.i])
                    .ok()
                    .and_then(|s| s.parse::<f64>().ok())
                    .map(Json::Num)
                    .ok_or_else(|| format!("数値が不正です (位置 {start})"))
            }
            None => Err("入力が途中で終わっています".into()),
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let s = self.b.get(self.i..self.i + 4).ok_or("\\u が途中で終わっています")?;
        self.i += 4;
        u32::from_str_radix(std::str::from_utf8(s).map_err(|e| e.to_string())?, 16).map_err(|e| e.to_string())
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let c = *self.b.get(self.i).ok_or("文字列が閉じていません")?;
            self.i += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let e = *self.b.get(self.i).ok_or("エスケープが途中で終わっています")?;
                    self.i += 1;
                    let ch = match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let hi = self.hex4()?;
                            let cp = if (0xD800..0xDC00).contains(&hi) {
                                if self.b.get(self.i) == Some(&b'\\') && self.b.get(self.i + 1) == Some(&b'u') {
                                    self.i += 2;
                                    let lo = self.hex4()?;
                                    0x10000 + ((hi - 0xD800) << 10) + (lo.wrapping_sub(0xDC00) & 0x3FF)
                                } else {
                                    0xFFFD
                                }
                            } else {
                                hi
                            };
                            char::from_u32(cp).unwrap_or('\u{FFFD}')
                        }
                        other => return Err(format!("未知のエスケープ \\{}", other as char)),
                    };
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                }
                _ => out.push(c),
            }
        }
        String::from_utf8(out).map_err(|e| e.to_string())
    }
}

pub fn parse_json(s: &str) -> Result<Json, String> {
    let mut p = Parser { b: s.as_bytes(), i: 0 };
    let v = p.value()?;
    p.ws();
    if p.i != p.b.len() {
        return Err(format!("末尾に余分な文字があります (位置 {})", p.i));
    }
    Ok(v)
}

// ---------------------------------------------------------------- golden

#[derive(Debug, Clone)]
struct GData {
    word: String,
    ruby: String,
    lcid: u32,
    rcid: u32,
    mid: u32,
    value: u32,
}

#[derive(Debug, Clone)]
struct GCand {
    text: String,
    value: u32,
    data: Vec<GData>,
}

#[derive(Debug)]
struct GRow {
    reading: String,
    seq: Vec<String>,
    main: Vec<GCand>,
    first_clause: Vec<GCand>,
}

fn hex(s: &str) -> u32 {
    u32::from_str_radix(s, 16).unwrap_or(0)
}

fn parse_cand(j: &Json) -> GCand {
    GCand {
        text: j.get("text").map_or("", Json::str).to_string(),
        value: hex(j.get("value").map_or("", Json::str)),
        data: j
            .get("data")
            .map_or(&[][..], Json::arr)
            .iter()
            .map(|d| GData {
                word: d.get("word").map_or("", Json::str).to_string(),
                ruby: d.get("ruby").map_or("", Json::str).to_string(),
                lcid: d.get("lcid").map_or(0.0, Json::num) as u32,
                rcid: d.get("rcid").map_or(0.0, Json::num) as u32,
                mid: d.get("mid").map_or(0.0, Json::num) as u32,
                value: hex(d.get("value").map_or("", Json::str)),
            })
            .collect(),
    }
}

fn load_golden(path: &Path) -> Result<Vec<GRow>, String> {
    let out = Command::new("gzip").arg("-dc").arg(path).output().map_err(|e| format!("gzip を実行できません: {e}"))?;
    if !out.status.success() {
        return Err(format!("gzip -dc {} に失敗しました", path.display()));
    }
    let text = String::from_utf8(out.stdout).map_err(|e| e.to_string())?;
    let mut rows = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let j = parse_json(line).map_err(|e| format!("{}:{}: {e}", path.display(), n + 1))?;
        rows.push(GRow {
            reading: j.get("reading").map_or("", Json::str).to_string(),
            seq: j.get("seq").map_or(&[][..], Json::arr).iter().map(|s| s.str().to_string()).collect(),
            main: j.get("main").map_or(&[][..], Json::arr).iter().map(parse_cand).collect(),
            first_clause: j.get("first_clause").map_or(&[][..], Json::arr).iter().map(parse_cand).collect(),
        });
    }
    Ok(rows)
}

/// unstable.txt の (読み, seq) 集合
fn load_unstable(dir: &Path) -> HashSet<(String, String)> {
    let mut set = HashSet::new();
    if let Ok(text) = std::fs::read_to_string(dir.join("unstable.txt")) {
        for line in text.lines() {
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }
            let cols: Vec<&str> = line.split('\t').collect();
            if cols.len() >= 2 {
                set.insert((cols[0].to_string(), cols[1].to_string()));
            }
        }
    }
    set
}

// ---------------------------------------------------------------- 比較

/// 比べるための (text, value の bit)
type View = (String, u32);

fn gview(c: &[GCand]) -> Vec<View> {
    c.iter().map(|c| (c.text.clone(), c.value)).collect()
}

fn rview(c: &[Cand]) -> Vec<View> {
    c.iter().map(|c| (c.text.clone(), c.value.to_bits())).collect()
}

/// value の bit が等しい連続区間 (集合として持つ)
fn runs(v: &[View]) -> Vec<(u32, Vec<&str>)> {
    let mut out: Vec<(u32, Vec<&str>)> = Vec::new();
    for (text, bits) in v {
        match out.last_mut() {
            Some((b, texts)) if b == bits => texts.push(text),
            _ => out.push((*bits, vec![text.as_str()])),
        }
    }
    for (_, texts) in &mut out {
        texts.sort_unstable();
    }
    out
}

/// 先頭 k 件を覆う区間までを比べる (同点の入れ替わりは無視)
fn top_equal(g: &[View], r: &[View], k: usize) -> bool {
    fn cover<'a>(v: &[(u32, Vec<&'a str>)], k: usize) -> Vec<(u32, Vec<&'a str>)> {
        let mut covered = 0;
        let mut out = Vec::new();
        for run in v {
            if covered >= k {
                break;
            }
            covered += run.1.len();
            out.push(run.clone());
        }
        out
    }
    cover(&runs(g), k) == cover(&runs(r), k)
}

fn full_exact(g: &[View], r: &[View]) -> bool {
    g == r
}

fn full_tie_aware(g: &[View], r: &[View]) -> bool {
    runs(g) == runs(r)
}

fn first_diff(g: &[View], r: &[View]) -> usize {
    g.iter().zip(r.iter()).position(|(a, b)| a != b).unwrap_or(g.len().min(r.len()))
}

// ---- golden の候補が特殊候補らしいか (tools/kkc_golden_candidates.py の special_kinds と同じ判定)

fn bits_to_f32(b: u32) -> f32 {
    f32::from_bits(b)
}

struct SpecialDetector {
    time: Regex,
    comma: Regex,
    era: Regex,
    unicode_reading: Regex,
}

impl SpecialDetector {
    fn new() -> Self {
        Self {
            time: Regex::new(r"^\d{1,2}:\d{2}$").unwrap(),
            comma: Regex::new(r"^-?\d{1,3}(,\d{3})+(\.\d+)?$").unwrap(),
            era: Regex::new(r"令和|平成|昭和|大正|明治|西暦|^\d+年$|元年").unwrap(),
            unicode_reading: Regex::new(r"^[uU]\+?[0-9a-fA-F]+$").unwrap(),
        }
    }

    /// 特殊候補の種類 (なければ None)
    fn kind(&self, c: &GCand, reading: &str) -> Option<&'static str> {
        if c.data.len() != 1 {
            return None;
        }
        let d = &c.data[0];
        let value = bits_to_f32(d.value);
        if c.text.starts_with("NovakeyR") {
            return Some("version");
        }
        if d.lcid == 0 && d.rcid == 0 {
            if c.text.contains('@') && reading.contains('@') {
                return Some("email");
            }
            if self.unicode_reading.is_match(reading) {
                return Some("unicode");
            }
        }
        if self.time.is_match(&c.text) && value == -10.0 {
            return Some("time");
        }
        if self.comma.is_match(&c.text) && value == -10.0 {
            return Some("comma");
        }
        if self.era.is_match(&c.text) {
            if value == -15.0 && d.lcid == 1288 {
                return Some("calendar");
            }
            if (value == -18.0 || value == -19.0) && d.lcid == 1285 && d.mid == 237 {
                return Some("calendar");
            }
        }
        None
    }
}

fn describe_g(c: &GCand) -> String {
    let data: Vec<String> = c
        .data
        .iter()
        .map(|d| format!("{}/{}({},{},{}){:.4}", d.word, d.ruby, d.lcid, d.rcid, d.mid, bits_to_f32(d.value)))
        .collect();
    format!("{:?} {:08x} ({:.4}) [{}]", c.text, c.value, bits_to_f32(c.value), data.join(" "))
}

fn describe_r(c: &Cand) -> String {
    let data: Vec<String> = c
        .data
        .iter()
        .map(|d| format!("{}/{}({},{},{}){:.4}", d.word, d.ruby, d.lcid, d.rcid, d.mid, d.value()))
        .collect();
    format!("{:?} {:08x} ({:.4}) [{}] src={}", c.text, c.value.to_bits(), c.value, data.join(" "), c.source.tag())
}

#[derive(Default)]
struct Tally {
    rows: usize,
    top1: usize,
    top3: usize,
    top9: usize,
    full_tie: usize,
    full_exact: usize,
    first_clause: usize,
}

impl Tally {
    fn line(&self, name: &str) -> String {
        let pct = |n: usize| if self.rows == 0 { 0.0 } else { 100.0 * n as f64 / self.rows as f64 };
        format!(
            "{name:10} rows {:5} | top1 {:5} ({:6.2}%) | top3 {:5} ({:6.2}%) | top9 {:5} ({:6.2}%) | full(tie) {:5} ({:6.2}%) | full(exact) {:5} ({:6.2}%) | first_clause {:5} ({:6.2}%)",
            self.rows,
            self.top1,
            pct(self.top1),
            self.top3,
            pct(self.top3),
            self.top9,
            pct(self.top9),
            self.full_tie,
            pct(self.full_tie),
            self.full_exact,
            pct(self.full_exact),
            self.first_clause,
            pct(self.first_clause),
        )
    }
}

struct Mismatch {
    severity: u8, // 0 = top3, 1 = top9, 2 = full
    label: String,
    class: &'static str,
    diff_index: usize,
    golden: Vec<GCand>,
    rust: Vec<Cand>,
}

/// `--kkc-compare` の本体。終了コード (0 = top3 が 100%)。
pub fn run(args: &[String]) -> i32 {
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let golden_path = PathBuf::from(arg("--golden").unwrap_or_else(|| "tools/golden/fresh.jsonl.gz".to_string()));
    let limit = arg("--limit").and_then(|s| s.parse::<usize>().ok()).unwrap_or(usize::MAX);
    let show = arg("--show").and_then(|s| s.parse::<usize>().ok()).unwrap_or(10);

    let core = match KkcCore::open() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let rows = match load_golden(&golden_path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let unstable = load_unstable(golden_path.parent().unwrap_or_else(|| Path::new(".")));
    let detector = SpecialDetector::new();

    let mut stable = Tally::default();
    let mut unstable_tally = Tally::default();
    let mut mismatches: Vec<Mismatch> = Vec::new();
    let mut class_counts: std::collections::BTreeMap<&'static str, usize> = std::collections::BTreeMap::new();
    let mut elapsed = std::time::Duration::ZERO;
    // golden にだけある特殊候補の種類ごとの (行数, top9 に入る行数)
    let mut special_rows: std::collections::BTreeMap<&'static str, (usize, usize)> = std::collections::BTreeMap::new();

    for row in rows.iter().take(limit) {
        let t0 = Instant::now();
        let Converted { main, first_clause } = core.convert_all(&row.reading);
        elapsed += t0.elapsed();
        let g = gview(&row.main);
        let r = rview(&main);
        let is_unstable = unstable.contains(&(row.reading.clone(), row.seq.join("|")));
        let t = if is_unstable { &mut unstable_tally } else { &mut stable };
        t.rows += 1;
        let ok1 = top_equal(&g, &r, 1);
        let ok3 = top_equal(&g, &r, 3);
        let ok9 = top_equal(&g, &r, 9);
        let ok_tie = full_tie_aware(&g, &r);
        let ok_exact = full_exact(&g, &r);
        t.top1 += ok1 as usize;
        t.top3 += ok3 as usize;
        t.top9 += ok9 as usize;
        t.full_tie += ok_tie as usize;
        t.full_exact += ok_exact as usize;
        t.first_clause += top_equal(&gview(&row.first_clause), &rview(&first_clause), 5) as usize;
        if ok_exact || is_unstable {
            continue;
        }
        {
            let rust_texts: HashSet<&str> = main.iter().map(|c| c.text.as_str()).collect();
            let mut any: HashSet<&'static str> = HashSet::new();
            let mut in_top9: HashSet<&'static str> = HashSet::new();
            for (rank, c) in row.main.iter().enumerate() {
                if let Some(kind) = detector.kind(c, &row.reading) {
                    if !rust_texts.contains(c.text.as_str()) {
                        any.insert(kind);
                        if rank < 9 {
                            in_top9.insert(kind);
                        }
                    }
                }
            }
            for kind in any {
                special_rows.entry(kind).or_default().0 += 1;
            }
            for kind in in_top9 {
                special_rows.entry(kind).or_default().1 += 1;
            }
        }
        // 分類: 特殊候補を除けば一致するか
        let g_wo_special: Vec<View> = row
            .main
            .iter()
            .filter(|c| detector.kind(c, &row.reading).is_none())
            .map(|c| (c.text.clone(), c.value))
            .collect();
        let class = if ok_tie {
            "tie-order"
        } else if full_tie_aware(&g_wo_special, &r) {
            "special-only"
        } else if top_equal(&g_wo_special, &r, 9) {
            "special+tail"
        } else {
            "other"
        };
        *class_counts.entry(class).or_default() += 1;
        let severity = if !ok3 {
            0
        } else if !ok9 {
            1
        } else {
            2
        };
        let label = if row.seq.is_empty() { row.reading.clone() } else { format!("{} [seq {}]", row.reading, row.seq.join("|")) };
        mismatches.push(Mismatch {
            severity,
            label,
            class,
            diff_index: first_diff(&g, &r),
            golden: row.main.clone(),
            rust: main,
        });
    }

    println!("golden: {}", golden_path.display());
    println!("{}", stable.line("stable"));
    println!("{}", unstable_tally.line("unstable"));
    println!("convert time: {:.1} ms total, {:.2} ms/row", elapsed.as_secs_f64() * 1000.0, elapsed.as_secs_f64() * 1000.0 / (stable.rows + unstable_tally.rows).max(1) as f64);
    println!("full-list mismatches by class (stable rows): {class_counts:?}");
    println!("golden-only special candidates (kind: rows missing it / of which in golden top9): {special_rows:?}");
    let by_severity = |s: u8| mismatches.iter().filter(|m| m.severity == s).count();
    println!(
        "mismatching stable rows: top3-fail {}, top9-fail-only {}, tail-only {}",
        by_severity(0),
        by_severity(1),
        by_severity(2)
    );

    mismatches.sort_by_key(|m| m.severity);
    for m in mismatches.iter().take(show) {
        println!();
        println!("--- {} (severity {}, class {}, first diff at #{})", m.label, ["top3", "top9", "tail"][m.severity as usize], m.class, m.diff_index);
        let from = m.diff_index.saturating_sub(1);
        for i in from..(m.diff_index + 2) {
            if let Some(c) = m.golden.get(i) {
                println!("  golden[{i}] {}", describe_g(c));
            }
            if let Some(c) = m.rust.get(i) {
                println!("  rust  [{i}] {}", describe_r(c));
            }
        }
        let g_texts: HashSet<&str> = m.golden.iter().map(|c| c.text.as_str()).collect();
        let r_texts: HashSet<&str> = m.rust.iter().map(|c| c.text.as_str()).collect();
        let only_g: Vec<String> = m
            .golden
            .iter()
            .filter(|c| !r_texts.contains(c.text.as_str()))
            .take(6)
            .map(|c| format!("#{} {}", m.golden.iter().position(|x| x.text == c.text).unwrap_or(0), describe_g(c)))
            .collect();
        let only_r: Vec<String> = m
            .rust
            .iter()
            .filter(|c| !g_texts.contains(c.text.as_str()))
            .take(6)
            .map(|c| format!("#{} {}", m.rust.iter().position(|x| x.text == c.text).unwrap_or(0), describe_r(c)))
            .collect();
        println!("  lengths: golden {} rust {}", m.golden.len(), m.rust.len());
        for o in only_g {
            println!("  golden-only {o}");
        }
        for o in only_r {
            println!("  rust-only   {o}");
        }
    }
    i32::from(stable.top3 != stable.rows)
}

/// `--kkc-convert <reading>`: Rust 版の候補を出どころ・値つきで表示する。
pub fn convert_cli(reading: &str, count: usize) -> i32 {
    let core = match KkcCore::open() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let t0 = Instant::now();
    let c = core.convert_all(reading);
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    for (i, cand) in c.main.iter().take(count).enumerate() {
        println!("{i}: {}", describe_r(cand));
    }
    println!("({} candidates, {ms:.2} ms)", c.main.len());
    0
}

/// `--kkc-bench <reading>`: 読みを 1 文字ずつ伸ばしながら変換した合計時間 (打鍵ごとの変換を模す)。
pub fn bench_cli(reading: &str) -> i32 {
    let core = match KkcCore::open() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let chars: Vec<char> = reading.chars().collect();
    let mut worst = 0.0f64;
    let t0 = Instant::now();
    for n in 1..=chars.len() {
        let prefix: String = chars[..n].iter().collect();
        let t = Instant::now();
        let _ = core.convert_all(&prefix);
        worst = worst.max(t.elapsed().as_secs_f64() * 1000.0);
    }
    println!(
        "{} chars: total {:.1} ms (worst single convert {:.1} ms, mean {:.2} ms)",
        chars.len(),
        t0.elapsed().as_secs_f64() * 1000.0,
        worst,
        t0.elapsed().as_secs_f64() * 1000.0 / chars.len().max(1) as f64
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[(&str, u32)]) -> Vec<View> {
        items.iter().map(|(t, b)| (t.to_string(), *b)).collect()
    }

    #[test]
    fn json_parser_reads_objects_arrays_and_escapes() {
        let j = parse_json(r#"{"a":[1,-2.5,"x\nあ😀"],"b":{"c":true,"d":null}}"#).unwrap();
        let a = j.get("a").unwrap().arr();
        assert_eq!(a[0].num(), 1.0);
        assert_eq!(a[1].num(), -2.5);
        assert_eq!(a[2].str(), "x\nあ😀");
        assert!(matches!(j.get("b").unwrap().get("c"), Some(Json::Bool)));
        assert!(parse_json("{").is_err());
        assert!(parse_json("[1] x").is_err());
    }

    #[test]
    fn ties_are_compared_as_sets_but_value_order_is_kept() {
        let g = v(&[("a", 1), ("b", 1), ("c", 2)]);
        let swapped = v(&[("b", 1), ("a", 1), ("c", 2)]);
        let different = v(&[("a", 1), ("c", 2), ("b", 1)]);
        assert!(top_equal(&g, &swapped, 3));
        assert!(full_tie_aware(&g, &swapped));
        assert!(!full_exact(&g, &swapped));
        assert!(!top_equal(&g, &different, 3));
        // k=1 でも、先頭を含む同点区間全体 (a, b) が揃わないと一致しない
        assert!(top_equal(&g, &swapped, 1));
        assert!(!top_equal(&g, &v(&[("a", 1), ("x", 1), ("c", 2)]), 1));
    }
}
