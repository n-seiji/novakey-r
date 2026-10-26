//! 審判の tokenizer.json (rinna/japanese-gpt2-xsmall の SentencePiece Unigram) を読んで id 列にする。
//!
//! `tokenizers` 0.21 の `Tokenizer::encode(text, false)` と同じ id を返すように、その流れをそのまま写している。
//! 1. 追加トークン (`<s>` など、normalized: false) を元の文字列から最左最長で切り出す (そのまま id)。
//! 2. 残りの区間ごとに Precompiled (`spm_precompiled` と同じ表引き) → 右端の空白を削る → 2 個以上の空白を `▁` 1 個に。
//! 3. Metaspace: 空白を `▁` にし、頭が `▁` でなければ `▁` を足し、各 `▁` の前で切る。
//! 4. 切った片ごとに Unigram の Viterbi (未知文字は min_score - 10、隣り合う未知はまとめて 1 個)。
//!
//! 読み込み時にファイルの構成がこの流れと一致するかを確かめ、違えば固定文字列のエラーにする
//! (別の構成のファイルを黙って違う id にしない)。
#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;
use unicode_segmentation::UnicodeSegmentation;

type R<T> = Result<T, &'static str>;

const META: char = '\u{2581}';
/// `tokenizers` の Unigram が未知文字に付ける減点 (`K_UNK_PENALTY`)。
const UNK_PENALTY: f64 = 10.0;
const NO_START: usize = usize::MAX;

pub struct Tokenizer {
    charsmap: Charsmap,
    /// 語彙 → (id, 対数確率)。
    vocab: HashMap<Box<str>, (u32, f64)>,
    /// 最長の語彙のバイト数 (これより長い部分文字列は引かない)。
    max_piece: usize,
    unk_id: u32,
    unk_score: f64,
    /// 元の文字列から切り出す追加トークン (内容, id)。
    added: Vec<(Box<str>, u32)>,
}

impl Tokenizer {
    pub fn from_file(path: &Path) -> R<Self> {
        let bytes = std::fs::read(path).map_err(|_| "cannot read tokenizer")?;
        let json: Value = serde_json::from_slice(&bytes).map_err(|_| "cannot parse tokenizer")?;
        Self::from_json(&json)
    }

    fn from_json(json: &Value) -> R<Self> {
        for key in ["truncation", "padding", "post_processor"] {
            if !json.get(key).is_none_or(Value::is_null) {
                return Err("unsupported tokenizer");
            }
        }
        let charsmap = Charsmap::parse(&base64_decode(normalizer_charsmap(json)?)?)?;
        check_pre_tokenizer(json)?;
        let model = &json["model"];
        if model["type"] != "Unigram" || !model.get("byte_fallback").is_none_or(|b| *b == false) {
            return Err("unsupported tokenizer model");
        }
        let entries = model["vocab"].as_array().ok_or("bad tokenizer vocab")?;
        let mut vocab = HashMap::with_capacity(entries.len());
        let mut min_score = f64::INFINITY;
        let mut max_piece = 0;
        for (id, e) in entries.iter().enumerate() {
            let piece = e.get(0).and_then(Value::as_str).ok_or("bad tokenizer vocab")?;
            let score = e.get(1).and_then(Value::as_f64).ok_or("bad tokenizer vocab")?;
            let id = u32::try_from(id).map_err(|_| "bad tokenizer vocab")?;
            // `tokenizers` は重複した語彙を後勝ちで引く。そうした語彙は扱わない。
            if piece.is_empty() || vocab.insert(piece.into(), (id, score)).is_some() {
                return Err("bad tokenizer vocab");
            }
            min_score = min_score.min(score);
            max_piece = max_piece.max(piece.len());
        }
        let unk_id = model["unk_id"].as_u64().and_then(|u| u32::try_from(u).ok()).ok_or("bad tokenizer unk")?;
        if unk_id as usize >= entries.len() {
            return Err("bad tokenizer unk");
        }
        let mut added = Vec::new();
        for t in json["added_tokens"].as_array().map_or(&[][..], Vec::as_slice) {
            let flag = |k: &str| t[k].as_bool().ok_or("bad added token");
            if flag("single_word")? || flag("lstrip")? || flag("rstrip")? || flag("normalized")? {
                return Err("unsupported added token");
            }
            let content = t["content"].as_str().filter(|c| !c.is_empty()).ok_or("bad added token")?;
            let id = t["id"].as_u64().and_then(|u| u32::try_from(u).ok()).ok_or("bad added token")?;
            added.push((content.into(), id));
        }
        Ok(Self { charsmap, vocab, max_piece, unk_id, unk_score: min_score - UNK_PENALTY, added })
    }

    /// 未知文字に当たる id。
    pub fn unk_id(&self) -> u32 {
        self.unk_id
    }

    /// 特殊トークンを足さずに id 列にする (`encode(text, false)` 相当)。
    pub fn encode(&self, text: &str) -> Vec<u32> {
        let mut out = Vec::new();
        let mut pos = 0;
        while let Some((start, len, id)) = self.next_added(text, pos) {
            self.encode_segment(&text[pos..start], &mut out);
            out.push(id);
            pos = start + len;
        }
        self.encode_segment(&text[pos..], &mut out);
        out
    }

    /// `pos` 以降で最も左に始まる追加トークン (同じ位置なら最長)。
    fn next_added(&self, text: &str, pos: usize) -> Option<(usize, usize, u32)> {
        let bytes = text.as_bytes();
        (pos..bytes.len()).find_map(|i| {
            self.added
                .iter()
                .filter(|(c, _)| bytes[i..].starts_with(c.as_bytes()))
                .max_by_key(|(c, _)| c.len())
                .map(|(c, id)| (i, c.len(), *id))
        })
    }

    fn encode_segment(&self, segment: &str, out: &mut Vec<u32>) {
        let normalized = self.charsmap.normalize(segment);
        let trimmed = normalized.trim_end_matches(char::is_whitespace);
        // Replace(" {2,}" → "▁") のあと Metaspace が残りの空白を `▁` にするので、空白の並びはどれも `▁` 1 個になる。
        let mut s = String::with_capacity(trimmed.len() + META.len_utf8());
        let mut in_spaces = false;
        for c in trimmed.chars() {
            if c == ' ' {
                if !in_spaces {
                    s.push(META);
                }
                in_spaces = true;
            } else {
                s.push(c);
                in_spaces = false;
            }
        }
        if s.is_empty() {
            return;
        }
        if !s.starts_with(META) {
            s.insert(0, META);
        }
        let mut start = 0;
        for (i, _) in s.match_indices(META).skip(1) {
            self.viterbi(&s[start..i], out);
            start = i;
        }
        self.viterbi(&s[start..], out);
    }

    /// `tokenizers` の `Unigram::encode_optimized` + `tokenize` と同じ最良分割。
    fn viterbi(&self, s: &str, out: &mut Vec<u32>) {
        // (その位置で終わる最良の id, 累積スコア, 始まりの位置)
        let mut best = vec![(0u32, 0f64, NO_START); s.len() + 1];
        let mut start = 0;
        while let Some(first) = s[start..].chars().next() {
            let till = best[start].1;
            let mblen = first.len_utf8();
            let mut single = false;
            let mut end = start;
            for c in s[start..].chars() {
                end += c.len_utf8();
                if end - start > self.max_piece {
                    break;
                }
                if let Some(&(id, score)) = self.vocab.get(&s[start..end]) {
                    let cand = score + till;
                    let t = &mut best[end];
                    if t.2 == NO_START || cand > t.1 {
                        *t = (id, cand, start);
                    }
                    single |= end - start == mblen;
                }
            }
            if !single {
                let cand = self.unk_score + till;
                let t = &mut best[start + mblen];
                if t.2 == NO_START || cand > t.1 {
                    *t = (self.unk_id, cand, start);
                }
            }
            start += mblen;
        }
        // 後ろからたどる。未知 (unk_id) が続く区間は 1 つの文字列にまとめてから語彙を引き直す。
        let mut ids = Vec::new();
        let mut end = s.len();
        let mut unk_end = None;
        while end > 0 {
            let (id, _, begin) = best[end];
            if id == self.unk_id {
                unk_end.get_or_insert(end);
            } else {
                if let Some(e) = unk_end.take() {
                    ids.push(self.lookup(&s[end..e]));
                }
                ids.push(id);
            }
            end = begin;
        }
        if let Some(e) = unk_end {
            ids.push(self.lookup(&s[..e]));
        }
        out.extend(ids.iter().rev());
    }

    fn lookup(&self, piece: &str) -> u32 {
        self.vocab.get(piece).map_or(self.unk_id, |&(id, _)| id)
    }
}

/// normalizer が Sequence[Precompiled, Strip(右だけ), Replace(" {2,}" → "▁")] であることを確かめ、charsmap を返す。
fn normalizer_charsmap(json: &Value) -> R<&str> {
    let n = &json["normalizer"];
    let steps = n["normalizers"].as_array().filter(|s| n["type"] == "Sequence" && s.len() == 3);
    let [pre, strip, replace] = steps.map(Vec::as_slice).ok_or("unsupported normalizer")? else {
        return Err("unsupported normalizer");
    };
    let strip_ok = strip["type"] == "Strip" && strip["strip_left"] == false && strip["strip_right"] == true;
    let replace_ok = replace["type"] == "Replace"
        && replace["pattern"]["Regex"] == " {2,}"
        && replace["content"] == META.to_string().as_str();
    if pre["type"] != "Precompiled" || !strip_ok || !replace_ok {
        return Err("unsupported normalizer");
    }
    pre["precompiled_charsmap"].as_str().ok_or("unsupported normalizer")
}

fn check_pre_tokenizer(json: &Value) -> R<()> {
    let p = &json["pre_tokenizer"];
    let ok = p["type"] == "Metaspace"
        && p["replacement"] == META.to_string().as_str()
        && p.get("prepend_scheme").is_none_or(|s| *s == "always")
        && p.get("add_prefix_space").is_none_or(|a| a.is_null() || *a == true)
        && p.get("split").is_none_or(|s| s.is_null() || *s == true);
    if ok {
        Ok(())
    } else {
        Err("unsupported pre_tokenizer")
    }
}

/// 標準の base64 (パディングあり / なし)。
fn base64_decode(s: &str) -> R<Vec<u8>> {
    let s = s.trim_end_matches('=');
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for b in s.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return Err("bad precompiled charsmap"),
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

/// SentencePiece の precompiled charsmap (ダブル配列の trie + NUL 区切りの正規化後文字列)。
/// `spm_precompiled` 0.1.4 と同じ引き方をする。
struct Charsmap {
    trie: Vec<u32>,
    normalized: Vec<u8>,
}

impl Charsmap {
    fn parse(blob: &[u8]) -> R<Self> {
        let word = |i: usize| blob.get(i..i + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
        let trie_size = word(0).ok_or("bad precompiled charsmap")? as usize;
        let trie = (0..trie_size / 4).map(|k| word(4 + 4 * k)).collect::<Option<Vec<_>>>().ok_or("bad precompiled charsmap")?;
        let normalized = blob[4 + 4 * trie.len()..].to_vec();
        if std::str::from_utf8(&normalized).is_err() {
            return Err("bad precompiled charsmap");
        }
        Ok(Self { trie, normalized })
    }

    /// `key` の接頭辞で最初に (最も短く) 見つかった項目の、正規化後文字列。
    fn transform(&self, key: &str) -> Option<&str> {
        let unit = |pos: usize| self.trie.get(pos).map(|&u| u as usize);
        let offset = |u: usize| (u >> 10) << ((u & (1 << 9)) >> 6);
        let mut pos = 0;
        pos ^= offset(unit(pos)?);
        for &c in key.as_bytes() {
            if c == 0 {
                break;
            }
            pos ^= c as usize;
            let u = unit(pos)?;
            if u & ((1 << 31) | 0xFF) != c as usize {
                return None;
            }
            pos ^= offset(u);
            if (u >> 8) & 1 == 1 {
                let index = unit(pos)? & ((1 << 31) - 1);
                let rest = self.normalized.get(index..)?;
                let len = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
                return std::str::from_utf8(&rest[..len]).ok();
            }
        }
        None
    }

    /// 書記素ごとに引き (6 バイト未満のとき)、なければ 1 文字ずつ引く。
    fn normalize(&self, s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for g in s.graphemes(true) {
            if g.len() < 6 {
                if let Some(n) = self.transform(g) {
                    out.push_str(n);
                    continue;
                }
            }
            for (i, c) in g.char_indices() {
                match self.transform(&g[i..i + c.len_utf8()]) {
                    Some(n) => out.push_str(n),
                    None => out.push(c),
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64は標準のアルファベットとパディングを読む() {
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64_decode("aGVsbG8").unwrap(), b"hello");
        assert_eq!(base64_decode("+/8=").unwrap(), [0xfb, 0xff]);
        assert!(base64_decode("a$==").is_err());
    }

    /// 何も正規化しない charsmap (trie は根だけ) と小さな語彙の tokenizer.json。
    fn small_json() -> Value {
        serde_json::json!({
            "version": "1.0", "truncation": null, "padding": null, "post_processor": null,
            "added_tokens": [{"id": 6, "content": "<s>", "single_word": false, "lstrip": false, "rstrip": false, "normalized": false, "special": true}],
            "normalizer": {"type": "Sequence", "normalizers": [
                {"type": "Precompiled", "precompiled_charsmap": "BAAAAAAAAAA="},
                {"type": "Strip", "strip_left": false, "strip_right": true},
                {"type": "Replace", "pattern": {"Regex": " {2,}"}, "content": "\u{2581}"}]},
            "pre_tokenizer": {"type": "Metaspace", "replacement": "\u{2581}", "prepend_scheme": "always", "split": true},
            "model": {"type": "Unigram", "unk_id": 0, "byte_fallback": false, "vocab": [
                ["<unk>", 0.0], ["\u{2581}", -1.0], ["a", -2.0], ["b", -2.0], ["ab", -3.0], ["\u{2581}ab", -3.5]]}
        })
    }

    #[test]
    fn 最良の分割を選び_続く未知文字は1つにまとめる() {
        let t = Tokenizer::from_json(&small_json()).unwrap();
        assert_eq!(t.encode("ab"), [5]);
        assert_eq!(t.encode("xyab"), [1, 0, 4]);
        assert_eq!(t.encode("ab  ab "), [5, 5]);
        assert_eq!(t.encode("ab<s>ab"), [5, 6, 5]);
        assert_eq!(t.encode("   "), Vec::<u32>::new());
        assert_eq!(t.encode(""), Vec::<u32>::new());
    }

    #[test]
    fn 語彙にない文字は_unk_id_として見分けられる() {
        let t = Tokenizer::from_json(&small_json()).unwrap();
        assert!(t.encode("xyab").contains(&t.unk_id()));
        assert!(!t.encode("ab ab").contains(&t.unk_id()));
    }

    #[test]
    fn 想定と違う構成のファイルは読み込まない() {
        let edits: [(&str, Value); 5] = [
            ("/model/byte_fallback", Value::Bool(true)),
            ("/model/type", "BPE".into()),
            ("/pre_tokenizer/prepend_scheme", "first".into()),
            ("/normalizer/normalizers/1/strip_left", Value::Bool(true)),
            ("/added_tokens/0/normalized", Value::Bool(true)),
        ];
        for (ptr, v) in edits {
            let mut j = small_json();
            *j.pointer_mut(ptr).unwrap() = v;
            assert!(Tokenizer::from_json(&j).is_err(), "{ptr}");
        }
        let mut j = small_json();
        j["post_processor"] = serde_json::json!({"type": "TemplateProcessing"});
        assert!(Tokenizer::from_json(&j).is_err(), "post_processor");
    }

    /// `tokenizers` 0.21.4 で作った (入力, id 列) と一致する (tokenizer.json は tools/judge.sha256 のもの)。
    /// tokenizer.json は git に入らないので、無ければ飛ばす (あるのに読めなければ失敗)。
    #[test]
    fn 本物の語彙で_tokenizers_crate_と同じ_id_列になる() {
        let dir = std::env::var_os("SHIZUKU_JUDGE_DIR")
            .map_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("resources/judge"), Into::into);
        let path = dir.join("tokenizer.json");
        if !path.exists() {
            eprintln!("skip: no tokenizer.json in resources/judge");
            return;
        }
        let t = Tokenizer::from_file(&path).expect("tokenizer.json exists but cannot be loaded");
        let golden = include_str!("../tools/tokenizer_ids.jsonl");
        let mut n = 0;
        for line in golden.lines() {
            let (input, want): (String, Vec<u32>) = serde_json::from_str(line).unwrap();
            assert_eq!(t.encode(&input), want, "line {}", n + 1);
            n += 1;
        }
        assert!(n >= 400, "{n}");
    }
}
