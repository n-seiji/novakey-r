//! azooKey 互換のかな漢字変換 (Swift 版 AzooKeyKanaKanjiConverter v0.11.2 の Rust 移植)。
//!
//! 辞書ファイル形式 (`dict` / `louds`)、ラティスと N-best Viterbi (`lattice`)、動的エントリ (`wise`)、
//! 候補の組み立て (`result`) を持つ。IME から使うのは `RustEngine` (環境変数 `NOVAKEY_ENGINE=rust` のとき)。
//! 学習・特殊候補は未実装 (P3 / P4)。
#![forbid(unsafe_code)]
#![allow(dead_code, unused_imports)]

pub mod check;
pub mod compare;
pub mod dict;
pub mod elem;
pub mod lattice;
pub mod louds;
pub mod result;
pub mod wise;

use std::cell::RefCell;
use std::rc::Rc;

use crate::composer::CandidateEngine;

pub use dict::{DictError, Dictionary, Entry};
pub use result::{Cand, Converted, Source};

/// 保持する N-best の数 (本番の `ConvertRequestOptions.N_best`)
const N_BEST: usize = 9;
/// IME に返す候補の最大数
const TOP_N: usize = 9;

/// 辞書と辞書引きのメモ。コントローラ (入力クライアント) 間で共有する。
pub struct KkcCore {
    dict: Dictionary,
    cache: RefCell<lattice::LookupCache>,
}

impl KkcCore {
    pub fn open() -> Result<Self, DictError> {
        Ok(Self { dict: Dictionary::open_default()?, cache: RefCell::new(lattice::LookupCache::new()) })
    }

    pub fn dictionary(&self) -> &Dictionary {
        &self.dict
    }

    /// 読み全体を変換する。結果の全件 (`mainResults` / `firstClauseResults`)。毎回ラティスを作り直す。
    pub fn convert_all(&self, reading: &str) -> Converted {
        if reading.is_empty() {
            return Converted::default();
        }
        let chars = lattice::katakana_chars(reading);
        let katakana: String = chars.iter().collect();
        let mut lat = lattice::Lattice::build(&self.dict, &mut self.cache.borrow_mut(), &chars);
        lat.run(&self.dict, N_BEST);
        result::process(&self.dict, &lat, &katakana)
    }
}

thread_local! {
    static CORE: RefCell<Option<Rc<KkcCore>>> = const { RefCell::new(None) };
}

/// スレッドごとの共有コア。開けなかったときはエラーを 1 度表示して `None`。
pub fn shared_core() -> Option<Rc<KkcCore>> {
    CORE.with(|c| {
        let mut slot = c.borrow_mut();
        if slot.is_none() {
            match KkcCore::open() {
                Ok(core) => *slot = Some(Rc::new(core)),
                Err(e) => {
                    eprintln!("novakey_r: 辞書を開けませんでした: {e}");
                    return None;
                }
            }
        }
        slot.clone()
    })
}

/// Rust 実装のかな漢字変換エンジン。
pub struct RustEngine {
    core: Option<Rc<KkcCore>>,
    /// 直近の変換の (revision, 返した候補数)。`select` の検証用。
    last: Option<(u64, usize)>,
}

impl RustEngine {
    pub fn new() -> Self {
        Self { core: shared_core(), last: None }
    }
}

impl Default for RustEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl CandidateEngine for RustEngine {
    fn convert(&mut self, revision: u64, hiragana: &str) -> Vec<String> {
        let Some(core) = &self.core else {
            return Vec::new();
        };
        if hiragana.is_empty() {
            return Vec::new();
        }
        let converted = core.convert_all(hiragana);
        let texts: Vec<String> =
            converted.main.iter().take(TOP_N).map(|c| c.text.replace('\u{1F}', "")).collect();
        self.last = Some((revision, texts.len()));
        texts
    }

    /// 学習は P4。ここでは revision と添字の検証だけして、状態を捨てる。
    fn select(&mut self, revision: u64, engine_index: usize) -> bool {
        let ok = matches!(self.last, Some((r, n)) if r == revision && engine_index < n);
        self.last = None;
        ok
    }

    fn cancel(&mut self) {
        self.last = None;
    }
}
