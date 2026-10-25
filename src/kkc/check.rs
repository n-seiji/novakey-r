//! 辞書リーダーの検証。`shizuku --kkc-check` とテストから使う。
//!
//! 1. containment: fresh golden の「data が 1 要素で ruby が読みの prefix」の候補が `prefix_entries` に含まれる
//! 2. self-consistency: 全 loudstxt3 の全スロットをデコードし、その ruby をトライでたどると同じノードに着く
//!    (全件) / `prefix_entries(ruby)` がその行を返す (サンプル)
#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use super::dict::{escaped_identifier, to_katakana, DictError, Dictionary, Entry};

/// golden から候補を TSV にする Python ヘルパー (列は `tools/kkc_golden_candidates.py` の docstring を参照)。
pub fn golden_candidates_tsv() -> Result<String, String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let out = Command::new("python3")
        .arg("-I")
        .arg(root.join("tools/kkc_golden_candidates.py"))
        .output()
        .map_err(|e| format!("python3 を起動できません: {e}"))?;
    if !out.status.success() {
        return Err(format!("kkc_golden_candidates.py が失敗しました: {}", String::from_utf8_lossy(&out.stderr)));
    }
    String::from_utf8(out.stdout).map_err(|e| e.to_string())
}

#[derive(Debug, Default)]
pub struct ContainmentReport {
    /// golden の対象行の総数
    pub total: usize,
    /// wise / special として除外した数 (種別ごと)
    pub excluded: BTreeMap<String, usize>,
    pub checked: usize,
    pub found: usize,
    /// 辞書に無いが、読みの表記違い (ひらがな・大文字など) を固有名詞・一般で包んだ候補 (processResult 由来)。
    /// 辞書にあれば `found` に数え、無くて派生形として説明できたものだけここに数える
    pub synthetic_variants: usize,
    /// adjust が 0 でなかった行 (想定外)
    pub nonzero_adjust: usize,
    /// 見つからなかった / 値が食い違った例
    pub misses: Vec<String>,
    pub elapsed: Duration,
}

fn hex_bits(s: &str) -> Option<u32> {
    u32::from_str_radix(s, 16).ok()
}

/// 固有名詞 (1288) / 一般 (501) で、word が ruby のひらがな化・大文字化・小文字化・先頭大文字化のどれか。
/// 数字記号以外の fallback 候補 (ruby 一致の保証、追加候補のひらがな・カタカナ・大文字) の形。
fn is_synthetic_variant(ruby: &str, word: &str, lcid: u16, rcid: u16, mid: u16) -> bool {
    if (lcid, rcid, mid) != (1288, 1288, 501) {
        return false;
    }
    let hira: String = ruby
        .chars()
        .map(|c| match c as u32 {
            0x30A1..=0x30F6 => char::from_u32(c as u32 - 0x60).unwrap_or(c),
            _ => c,
        })
        .collect();
    let mut cap = ruby.to_lowercase();
    if let Some(first) = cap.get(..1) {
        cap = first.to_uppercase() + &cap[1..];
    }
    [ruby.to_string(), hira.clone(), ruby.to_uppercase(), hira.to_uppercase(), ruby.to_lowercase(), cap]
        .iter()
        .any(|v| v == word)
}

pub fn containment(dict: &Dictionary, tsv: &str) -> Result<ContainmentReport, DictError> {
    let start = Instant::now();
    let mut report = ContainmentReport::default();
    let mut cache: BTreeMap<String, Vec<Entry>> = BTreeMap::new();
    for line in tsv.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() != 9 {
            continue;
        }
        report.total += 1;
        if f[8] != "-" {
            *report.excluded.entry(f[8].to_string()).or_default() += 1;
            continue;
        }
        let (Ok(lcid), Ok(rcid), Ok(mid), Some(vbits), Some(abits)) =
            (f[3].parse::<u16>(), f[4].parse::<u16>(), f[5].parse::<u16>(), hex_bits(f[6]), hex_bits(f[7]))
        else {
            report.misses.push(format!("パース不能: {line}"));
            continue;
        };
        report.checked += 1;
        if abits != 0 {
            report.nonzero_adjust += 1;
        }
        if !cache.contains_key(f[0]) {
            cache.insert(f[0].to_string(), dict.prefix_entries(f[0])?);
        }
        let entries = &cache[f[0]];
        // golden の value は DicdataElement.value() = min(0, base + adjust)
        let adjust = f32::from_bits(abits);
        let hit = entries.iter().any(|e| {
            e.ruby == f[1]
                && e.word == f[2]
                && e.lcid == lcid
                && e.rcid == rcid
                && e.mid == mid
                && (e.value + adjust).min(0.0).to_bits() == vbits
        });
        if hit {
            report.found += 1;
        } else if is_synthetic_variant(f[1], f[2], lcid, rcid, mid) {
            report.synthetic_variants += 1;
        } else {
            report.misses.push(line.to_string());
        }
    }
    report.elapsed = start.elapsed();
    Ok(report)
}

#[derive(Debug, Default)]
pub struct SelfReport {
    pub files: usize,
    pub nodes_with_rows: usize,
    pub rows: usize,
    /// ruby をトライでたどって同じノードに着いた数
    pub trie_ok: usize,
    pub sampled: usize,
    pub sample_ok: usize,
    pub failures: Vec<String>,
    pub decode_elapsed: Duration,
    pub sample_elapsed: Duration,
}

/// `sample_step` 件に 1 件の割合 (行のあるノード単位) で `prefix_entries` による検索も確認する。
pub fn self_consistency(dict: &Dictionary, sample_step: usize) -> Result<SelfReport, DictError> {
    let mut r = SelfReport::default();
    let step = sample_step.max(1);
    let chid = std::fs::read_to_string(dict.dir().join("louds/charID.chid"))
        .map_err(|source| DictError::Io { path: dict.dir().join("louds/charID.chid"), source })?;
    let mut counter = 0usize;
    for first in chid.chars() {
        let id = escaped_identifier(&first.to_string());
        if !dict.dir().join(format!("louds/{id}.louds")).is_file() {
            continue;
        }
        let Some(trie) = dict.trie(first)? else { continue };
        r.files += 1;
        let mut shard_index = 0;
        loop {
            let t0 = Instant::now();
            let Some(shard) = dict.shard(first, shard_index)? else { break };
            let mut sample: Vec<(usize, Vec<Entry>)> = Vec::new();
            for slot in 0..shard.slot_count() {
                let entries = shard.slot(slot)?;
                if entries.is_empty() {
                    continue;
                }
                let node = (shard_index << 11) | slot;
                r.nodes_with_rows += 1;
                r.rows += entries.len();
                let ruby = entries[0].ruby.clone();
                let ids: Vec<u8> = ruby.chars().map(|c| dict.char_id(c)).collect();
                if entries.iter().any(|e| e.ruby != ruby) {
                    r.failures.push(format!("node {node}: スロット内で ruby が異なる ({ruby})"));
                } else if !ruby.starts_with(first) {
                    r.failures.push(format!("node {node}: ruby {ruby} が先頭文字 {first} で始まらない"));
                } else if trie.search(&ids) == Some(node) {
                    r.trie_ok += 1;
                } else {
                    r.failures.push(format!("node {node}: ruby {ruby} をたどると {:?}", trie.search(&ids)));
                }
                if counter % step == 0 {
                    sample.push((node, entries));
                }
                counter += 1;
            }
            r.decode_elapsed += t0.elapsed();

            let t1 = Instant::now();
            for (node, entries) in sample {
                r.sampled += 1;
                let got = dict.prefix_entries(&entries[0].ruby)?;
                let got_set: HashSet<(&str, &str, u16, u16, u16, u32)> =
                    got.iter().map(|e| (e.ruby.as_str(), e.word.as_str(), e.lcid, e.rcid, e.mid, e.value.to_bits())).collect();
                let all = entries
                    .iter()
                    .all(|e| got_set.contains(&(e.ruby.as_str(), e.word.as_str(), e.lcid, e.rcid, e.mid, e.value.to_bits())));
                if all {
                    r.sample_ok += 1;
                } else {
                    r.failures.push(format!("node {node}: prefix_entries({}) が行を返さない", entries[0].ruby));
                }
            }
            r.sample_elapsed += t1.elapsed();
            shard_index += 1;
        }
    }
    r.failures.truncate(30);
    Ok(r)
}

/// `--kkc-check [dictionary-dir]`
pub fn run_cli(dir: Option<&str>) -> i32 {
    match run(dir) {
        Ok(ok) => i32::from(!ok),
        Err(e) => {
            eprintln!("kkc-check: {e}");
            1
        }
    }
}

fn run(dir: Option<&str>) -> Result<bool, String> {
    let t = Instant::now();
    let dict = match dir {
        Some(d) => Dictionary::open(Path::new(d)),
        None => Dictionary::open_default(),
    }
    .map_err(|e| e.to_string())?;
    println!("dictionary: {}", dict.dir().display());
    println!("cold open (charID + mm): {:?}", t.elapsed());

    let tsv = golden_candidates_tsv()?;
    let c = containment(&dict, &tsv).map_err(|e| e.to_string())?;
    let excluded_total: usize = c.excluded.values().sum();
    println!("\n== containment (fresh golden) ==");
    println!("single-data prefix candidates: {}", c.total);
    println!("excluded (wise/special): {excluded_total} {:?}", c.excluded);
    println!(
        "checked: {}  found: {}  synthetic variants (not in dict): {}  other misses: {}  nonzero adjust: {}  ({:?})",
        c.checked, c.found, c.synthetic_variants, c.misses.len(), c.nonzero_adjust, c.elapsed
    );
    for m in &c.misses {
        println!("  MISS {m}");
    }

    let dict2 = dir.map_or_else(Dictionary::open_default, |d| Dictionary::open(Path::new(d))).map_err(|e| e.to_string())?;
    let s = self_consistency(&dict2, 50).map_err(|e| e.to_string())?;
    println!("\n== self-consistency ==");
    println!("trie files: {}  nodes with rows: {}  rows: {}", s.files, s.nodes_with_rows, s.rows);
    println!("trie path ok: {}/{}  (decode+walk {:?})", s.trie_ok, s.nodes_with_rows, s.decode_elapsed);
    println!("prefix_entries sample ok: {}/{}  ({:?})", s.sample_ok, s.sampled, s.sample_elapsed);
    for f in &s.failures {
        println!("  FAIL {f}");
    }

    // 典型的な検索時間
    let readings = ["キョウハイイテンキデスネ", "ニホンゴニュウリョク", "アシタ", "ガッコウ"];
    let t = Instant::now();
    let mut n = 0;
    for r in readings {
        n += dict.prefix_entries(&to_katakana(r)).map_err(|e| e.to_string())?.len();
    }
    println!("\nprefix_entries x{} (warm shards, cold first call included): {:?} ({} entries)", readings.len(), t.elapsed(), n);
    let t = Instant::now();
    for _ in 0..100 {
        for r in readings {
            std::hint::black_box(dict.prefix_entries(r).map_err(|e| e.to_string())?.len());
        }
    }
    println!("prefix_entries warm: {:?} per call", t.elapsed() / (100 * readings.len() as u32));

    Ok(c.found + c.synthetic_variants == c.checked && c.nonzero_adjust == 0 && c.misses.is_empty() && s.failures.is_empty() && s.trie_ok == s.nodes_with_rows && s.sample_ok == s.sampled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golden_single_data_candidates_are_found_in_the_dictionary() {
        let dict = Dictionary::open_default().unwrap();
        let tsv = golden_candidates_tsv().unwrap();
        let r = containment(&dict, &tsv).unwrap();
        assert!(r.checked > 100_000, "checked={}", r.checked);
        assert_eq!(r.nonzero_adjust, 0);
        assert!(r.misses.is_empty(), "{:#?}", r.misses);
        assert_eq!(r.found + r.synthetic_variants, r.checked);
        assert!(r.found > 120_000, "found={}", r.found);
    }

    #[test]
    fn every_decoded_row_is_reachable_by_its_ruby() {
        let dict = Dictionary::open_default().unwrap();
        let r = self_consistency(&dict, 200).unwrap();
        assert!(r.failures.is_empty(), "{:#?}", r.failures);
        assert!(r.files > 100, "files={}", r.files);
        assert_eq!(r.trie_ok, r.nodes_with_rows);
        assert_eq!(r.sample_ok, r.sampled);
    }
}
