//! 辞書ディレクトリ (`dictionary/`) の遅延読み込みと検索。
//!
//! レイアウト:
//! - `louds/charID.chid`            文字 -> 文字 ID (行内の位置)。未知は 255
//! - `louds/[XXXX].louds` `.loudschars2`  先頭 1 文字ごとのトライ
//! - `louds/[XXXX]{shard}.loudstxt3`      ノード n は shard `n >> 11`、スロット `n & 2047`
//! - `cb/{lcid}.binary`             品詞連接コスト (疎な行)
//! - `mm.binary`                    意味連接コスト (502 x 502 の f32)
#![forbid(unsafe_code)]

use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use super::louds::Louds;

const SHARD_SHIFT: usize = 11;
const LOCAL_MASK: usize = (1 << SHARD_SHIFT) - 1;
/// 品詞 ID の数 (`DicdataStore.cidCount`)
pub const CID_COUNT: usize = 1319;
/// 意味 ID の数 (`DicdataStore.midCount`)
pub const MID_COUNT: usize = 502;
/// 連接コストの行が無いときの値
const CC_MISSING: f32 = -25.0;
const UNKNOWN_CHAR_ID: u8 = 255;

#[derive(Debug)]
pub enum DictError {
    /// 辞書ディレクトリが見つからない / 不完全
    NotFound(String),
    Io { path: PathBuf, source: io::Error },
    Corrupt { path: PathBuf, reason: String },
}

impl fmt::Display for DictError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DictError::NotFound(msg) => write!(f, "辞書が見つかりません: {msg}"),
            DictError::Io { path, source } => write!(f, "辞書の読み込みに失敗しました ({}): {source}", path.display()),
            DictError::Corrupt { path, reason } => write!(f, "辞書の形式が不正です ({}): {reason}", path.display()),
        }
    }
}

impl std::error::Error for DictError {}

/// 辞書の 1 エントリ。`ruby` はカタカナ。
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub ruby: String,
    pub word: String,
    pub lcid: u16,
    pub rcid: u16,
    pub mid: u16,
    /// 辞書に入っている基本コスト (`DicdataElement.baseValue`)
    pub value: f32,
}

/// ひらがなをカタカナにする (U+3041..=U+3096 に +0x60)。`String.toKatakana()` と同じ。
pub fn to_katakana(s: &str) -> String {
    s.chars()
        .map(|c| match c as u32 {
            0x3041..=0x3096 => char::from_u32(c as u32 + 0x60).unwrap_or(c),
            _ => c,
        })
        .collect()
}

/// `DictionaryBuilder.escapedIdentifier`: UTF-16 コードユニットを `%04X` にして `_` でつなぎ `[]` で囲む。
pub fn escaped_identifier(id: &str) -> String {
    match id {
        "user" | "memory" | "user_shortcuts" => id.to_string(),
        _ => {
            let chunks: Vec<String> = id.encode_utf16().map(|u| format!("{u:04X}")).collect();
            format!("[{}]", chunks.join("_"))
        }
    }
}

/// 辞書ディレクトリを探す。
/// 1. 環境変数 `NOVAKEY_DICTIONARY` (指定されているのに不正ならエラー。黙ってフォールバックしない)
/// 2. 実行ファイルの `../Resources/dictionary` (アプリバンドル)
/// 3. ビルド時のリポジトリの `dictionary/` (テストとデバッグビルドだけ。リリースビルドには入れない。
///    バンドルへのコピー漏れを開発中の環境で見逃さないため。CLI の開発ツールは `NOVAKEY_DICTIONARY` で渡す)
pub fn locate_dictionary_dir() -> Result<PathBuf, DictError> {
    if let Some(v) = std::env::var_os("NOVAKEY_DICTIONARY") {
        let p = PathBuf::from(v);
        return if is_dictionary_dir(&p) {
            Ok(p)
        } else {
            Err(DictError::NotFound(format!("NOVAKEY_DICTIONARY={} は辞書ディレクトリではありません", p.display())))
        };
    }
    let mut tried = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("../Resources/dictionary");
            if is_dictionary_dir(&p) {
                return Ok(p);
            }
            tried.push(p);
        }
    }
    #[cfg(any(test, debug_assertions))]
    {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("dictionary");
        if is_dictionary_dir(&p) {
            return Ok(p);
        }
        tried.push(p);
    }
    let list: Vec<String> = tried.iter().map(|p| p.display().to_string()).collect();
    Err(DictError::NotFound(format!("探した場所: {}", list.join(", "))))
}

fn is_dictionary_dir(p: &Path) -> bool {
    p.join("louds/charID.chid").is_file() && p.join("mm.binary").is_file()
}

/// 存在しないファイルは `None`、それ以外のエラーは `Err`。
fn read_file_optional(path: &Path) -> Result<Option<Vec<u8>>, DictError> {
    match fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(DictError::Io { path: path.to_path_buf(), source }),
    }
}

fn le_u16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn le_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn le_f32(b: &[u8], at: usize) -> f32 {
    f32::from_bits(le_u32(b, at))
}

/// 1 つの `.loudstxt3` ファイル。
#[derive(Debug)]
pub struct Shard {
    bytes: Vec<u8>,
    path: PathBuf,
    slot_count: usize,
}

impl Shard {
    pub fn from_bytes(bytes: Vec<u8>, path: PathBuf) -> Result<Self, DictError> {
        if bytes.len() < 2 {
            return Err(DictError::Corrupt { path, reason: "ヘッダが短すぎます".into() });
        }
        let slot_count = le_u16(&bytes, 0) as usize;
        if bytes.len() < 2 + slot_count * 4 {
            return Err(DictError::Corrupt { path, reason: "オフセット表が途中で切れています".into() });
        }
        Ok(Self { bytes, path, slot_count })
    }

    pub fn slot_count(&self) -> usize {
        self.slot_count
    }

    /// スロット `idx` のエントリ。範囲外のスロットは空。
    pub fn slot(&self, idx: usize) -> Result<Vec<Entry>, DictError> {
        if idx >= self.slot_count {
            return Ok(Vec::new());
        }
        let start = le_u32(&self.bytes, 2 + idx * 4) as usize;
        let end = if idx + 1 == self.slot_count {
            self.bytes.len()
        } else {
            le_u32(&self.bytes, 2 + (idx + 1) * 4) as usize
        };
        if start > end || end > self.bytes.len() {
            return Err(self.corrupt(format!("スロット {idx} のオフセットが不正です ({start}..{end})")));
        }
        self.parse_slot(&self.bytes[start..end])
    }

    fn corrupt(&self, reason: String) -> DictError {
        DictError::Corrupt { path: self.path.clone(), reason }
    }

    /// `u16 行数` + 行 x 10 byte (`u16 lcid, u16 rcid, u16 mid, f32 value`) + UTF-8 `ruby\tword1\tword2...`。
    /// word が空文字なら ruby と同じ。
    fn parse_slot(&self, b: &[u8]) -> Result<Vec<Entry>, DictError> {
        if b.len() < 2 {
            return Err(self.corrupt("スロットが 2 byte 未満です".into()));
        }
        let count = le_u16(b, 0) as usize;
        let rows_end = 2 + count * 10;
        if b.len() < rows_end {
            return Err(self.corrupt("行データが途中で切れています".into()));
        }
        let text = std::str::from_utf8(&b[rows_end..]).map_err(|e| self.corrupt(format!("UTF-8 が不正です: {e}")))?;
        if count == 0 {
            return Ok(Vec::new());
        }
        let mut fields = text.split('\t');
        let ruby = fields.next().unwrap_or("");
        let words: Vec<&str> = fields.collect();
        if words.len() != count {
            return Err(self.corrupt(format!("行数 {count} に対して語が {} 個です (ruby {ruby:?})", words.len())));
        }
        Ok(words
            .into_iter()
            .enumerate()
            .map(|(i, w)| {
                let at = 2 + i * 10;
                Entry {
                    ruby: ruby.to_string(),
                    word: if w.is_empty() { ruby.to_string() } else { w.to_string() },
                    lcid: le_u16(b, at),
                    rcid: le_u16(b, at + 2),
                    mid: le_u16(b, at + 4),
                    value: le_f32(b, at + 6),
                }
            })
            .collect())
    }
}

/// 辞書。ファイルは必要になったときに読み込む (単一スレッド前提。`RefCell` を使う)。
pub struct Dictionary {
    dir: PathBuf,
    char_ids: HashMap<char, u8>,
    mm: Vec<f32>,
    tries: RefCell<HashMap<char, Option<Rc<Louds>>>>,
    shards: RefCell<HashMap<(char, usize), Option<Rc<Shard>>>>,
    cc_rows: Vec<OnceCell<Option<Box<[f32]>>>>,
}

impl fmt::Debug for Dictionary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dictionary").field("dir", &self.dir).finish_non_exhaustive()
    }
}

impl Dictionary {
    /// 既定の場所 (`locate_dictionary_dir`) から開く。
    pub fn open_default() -> Result<Self, DictError> {
        Self::open(&locate_dictionary_dir()?)
    }

    /// `charID.chid` と `mm.binary` だけを読み、残りは遅延読み込み。
    pub fn open(dir: &Path) -> Result<Self, DictError> {
        let chid_path = dir.join("louds/charID.chid");
        let chid = match read_file_optional(&chid_path)? {
            Some(b) => b,
            None => return Err(DictError::NotFound(format!("{} がありません", chid_path.display()))),
        };
        let chid = String::from_utf8(chid)
            .map_err(|e| DictError::Corrupt { path: chid_path.clone(), reason: e.to_string() })?;
        let mut char_ids = HashMap::new();
        for (i, c) in chid.chars().enumerate() {
            let id = u8::try_from(i)
                .ok()
                .filter(|&id| id != UNKNOWN_CHAR_ID)
                .ok_or_else(|| DictError::Corrupt { path: chid_path.clone(), reason: "文字が 255 個以上あります".into() })?;
            if char_ids.insert(c, id).is_some() {
                return Err(DictError::Corrupt { path: chid_path, reason: format!("文字 {c:?} が重複しています") });
            }
        }

        let mm_path = dir.join("mm.binary");
        let mm_bytes = read_file_optional(&mm_path)?
            .ok_or_else(|| DictError::NotFound(format!("{} がありません", mm_path.display())))?;
        if mm_bytes.len() % 4 != 0 || mm_bytes.len() / 4 < MID_COUNT * MID_COUNT {
            return Err(DictError::Corrupt { path: mm_path, reason: format!("サイズが不正です ({} bytes)", mm_bytes.len()) });
        }
        let mm = mm_bytes.chunks_exact(4).map(|c| f32::from_bits(u32::from_le_bytes([c[0], c[1], c[2], c[3]]))).collect();

        Ok(Self {
            dir: dir.to_path_buf(),
            char_ids,
            mm,
            tries: RefCell::new(HashMap::new()),
            shards: RefCell::new(HashMap::new()),
            cc_rows: (0..CID_COUNT).map(|_| OnceCell::new()).collect(),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 文字 ID。未知の文字は 255。
    pub fn char_id(&self, c: char) -> u8 {
        self.char_ids.get(&c).copied().unwrap_or(UNKNOWN_CHAR_ID)
    }

    /// `first` で始まる語のトライ。その文字のファイルが無ければ `None`。
    pub fn trie(&self, first: char) -> Result<Option<Rc<Louds>>, DictError> {
        if let Some(t) = self.tries.borrow().get(&first) {
            return Ok(t.clone());
        }
        let id = escaped_identifier(&first.to_string());
        let louds_path = self.dir.join(format!("louds/{id}.louds"));
        let chars_path = self.dir.join(format!("louds/{id}.loudschars2"));
        let loaded = match (read_file_optional(&louds_path)?, read_file_optional(&chars_path)?) {
            (Some(l), Some(c)) => Some(Rc::new(
                Louds::from_bytes(&l, c).map_err(|reason| DictError::Corrupt { path: louds_path, reason })?,
            )),
            _ => None,
        };
        self.tries.borrow_mut().insert(first, loaded.clone());
        Ok(loaded)
    }

    /// `first` のトライのノード `shard_index` 番目の shard ファイル。無ければ `None`。
    pub fn shard(&self, first: char, shard_index: usize) -> Result<Option<Rc<Shard>>, DictError> {
        if let Some(s) = self.shards.borrow().get(&(first, shard_index)) {
            return Ok(s.clone());
        }
        let id = escaped_identifier(&first.to_string());
        let path = self.dir.join(format!("louds/{id}{shard_index}.loudstxt3"));
        let loaded = match read_file_optional(&path)? {
            Some(b) => Some(Rc::new(Shard::from_bytes(b, path)?)),
            None => None,
        };
        self.shards.borrow_mut().insert((first, shard_index), loaded.clone());
        Ok(loaded)
    }

    /// トライのノードに対応するエントリ。
    pub fn node_entries(&self, first: char, node: usize) -> Result<Vec<Entry>, DictError> {
        match self.shard(first, node >> SHARD_SHIFT)? {
            Some(s) => s.slot(node & LOCAL_MASK),
            None => Ok(Vec::new()),
        }
    }

    /// カタカナの読みの、先頭から始まる全ての prefix に一致するエントリ (短い prefix から順)。
    /// `movingTowardPrefixSearch` + `getDicdataFromLoudstxt3` の first-character トライ分に当たる。
    /// user / memory 辞書は対象外。読みはカタカナ化済みであること (`to_katakana`)。
    pub fn prefix_entries(&self, katakana_reading: &str) -> Result<Vec<Entry>, DictError> {
        let chars: Vec<char> = katakana_reading.chars().collect();
        let Some(&first) = chars.first() else {
            return Ok(Vec::new());
        };
        let Some(trie) = self.trie(first)? else {
            return Ok(Vec::new());
        };
        let ids: Vec<u8> = chars.iter().map(|&c| self.char_id(c)).collect();
        let mut out = Vec::new();
        for (_depth, node) in trie.walk(&ids) {
            out.extend(self.node_entries(first, node)?);
        }
        Ok(out)
    }

    /// 品詞連接コスト。`former` は左の語の rcid、`latter` は右の語の lcid。
    /// 行が無い・読めない・範囲外は -25 (Swift 版と同じ)。
    pub fn cc(&self, rcid: usize, lcid: usize) -> f32 {
        let Some(cell) = self.cc_rows.get(rcid) else {
            return CC_MISSING;
        };
        let row = cell.get_or_init(|| self.load_cc_row(rcid).ok().flatten());
        match row {
            Some(r) => r.get(lcid).copied().unwrap_or(CC_MISSING),
            None => CC_MISSING,
        }
    }

    /// 左の語の rcid の連接コスト行 (`getCCLatter`)。行が無ければ `None` で、呼び出し側は -25 とみなす。
    pub fn cc_row(&self, rcid: usize) -> Option<&[f32]> {
        let cell = self.cc_rows.get(rcid)?;
        cell.get_or_init(|| self.load_cc_row(rcid).ok().flatten()).as_deref()
    }

    /// `cb/{rcid}.binary` = (i32 key, f32 value) の並び。最初が key -1 の行既定値、残りが疎な上書き。
    pub fn load_cc_row(&self, rcid: usize) -> Result<Option<Box<[f32]>>, DictError> {
        let path = self.dir.join(format!("cb/{rcid}.binary"));
        let Some(bytes) = read_file_optional(&path)? else {
            return Ok(None);
        };
        if bytes.is_empty() {
            return Ok(None);
        }
        if bytes.len() % 8 != 0 {
            return Err(DictError::Corrupt { path, reason: format!("サイズが 8 の倍数ではありません ({} bytes)", bytes.len()) });
        }
        Ok(Some(parse_cc_row(&bytes).map_err(|reason| DictError::Corrupt { path, reason })?))
    }

    /// 意味連接コスト。どちらかが 500 なら 0。
    pub fn mm(&self, former: usize, latter: usize) -> f32 {
        if former == 500 || latter == 500 {
            return 0.0;
        }
        if former >= MID_COUNT || latter >= MID_COUNT {
            return 0.0;
        }
        self.mm[former * MID_COUNT + latter]
    }
}

fn parse_cc_row(bytes: &[u8]) -> Result<Box<[f32]>, String> {
    let key0 = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    if key0 != -1 {
        return Err(format!("先頭のキーが -1 ではありません ({key0})"));
    }
    let mut row = vec![le_f32(bytes, 4); CID_COUNT];
    for pair in bytes[8..].chunks_exact(8) {
        let key = i32::from_le_bytes([pair[0], pair[1], pair[2], pair[3]]);
        let slot = usize::try_from(key)
            .ok()
            .filter(|&k| k < CID_COUNT)
            .ok_or_else(|| format!("キーが範囲外です ({key})"))?;
        row[slot] = le_f32(pair, 4);
    }
    Ok(row.into_boxed_slice())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict() -> Dictionary {
        Dictionary::open_default().expect("辞書を開けること")
    }

    #[test]
    fn escaped_identifier_uses_utf16_units() {
        assert_eq!(escaped_identifier("あ"), "[3042]");
        assert_eq!(escaped_identifier("AB"), "[0041_0042]");
        assert_eq!(escaped_identifier("🇯🇵"), "[D83C_DDEF_D83C_DDF5]");
        assert_eq!(escaped_identifier("\u{20BB7}"), "[D842_DFB7]");
        assert_eq!(escaped_identifier("user"), "user");
        assert_eq!(escaped_identifier("memory"), "memory");
    }

    #[test]
    fn to_katakana_covers_the_hiragana_block_only() {
        assert_eq!(to_katakana("きょうはゔぁ"), "キョウハヴァ");
        assert_eq!(to_katakana("ゔ"), "ヴ");
        assert_eq!(to_katakana("aー1漢"), "aー1漢");
    }

    #[test]
    fn unknown_char_maps_to_255() {
        let d = dict();
        assert_eq!(d.char_id('😀'), 255);
        assert_eq!(d.char_id('\u{10FFFF}'), 255);
        assert_ne!(d.char_id('ア'), 255);
        // chid の先頭は NUL: 位置がそのまま ID
        assert_eq!(d.char_id('\0'), 0);
    }

    #[test]
    fn slot_parsing_decodes_rows_and_empty_words() {
        // 2 行: (1,2,3,-1.5) と (4,5,6,-2.0)。word は "漢" と空 (= ruby)
        let mut b = vec![2, 0];
        for (l, r, m, v) in [(1u16, 2u16, 3u16, -1.5f32), (4, 5, 6, -2.0)] {
            b.extend(l.to_le_bytes());
            b.extend(r.to_le_bytes());
            b.extend(m.to_le_bytes());
            b.extend(v.to_le_bytes());
        }
        b.extend("カン\t漢\t".as_bytes());
        // 2 スロット: [0] = 空 (u16 0)、[1] = 上のデータ
        let mut file = vec![2, 0];
        file.extend(10u32.to_le_bytes());
        file.extend(12u32.to_le_bytes());
        file.extend([0, 0]);
        file.extend(&b);
        let shard = Shard::from_bytes(file, PathBuf::from("test.loudstxt3")).unwrap();
        assert_eq!(shard.slot(0).unwrap(), vec![]);
        let e = shard.slot(1).unwrap();
        assert_eq!(e.len(), 2);
        assert_eq!((e[0].ruby.as_str(), e[0].word.as_str(), e[0].lcid, e[0].rcid, e[0].mid, e[0].value), ("カン", "漢", 1, 2, 3, -1.5));
        assert_eq!((e[1].word.as_str(), e[1].value), ("カン", -2.0));
        assert_eq!(shard.slot(2).unwrap(), vec![]);
    }

    #[test]
    fn slot_with_wrong_word_count_is_an_error() {
        let mut b = vec![1, 0, 1, 0, 1, 0, 1, 0, 0, 0, 0, 0];
        b.extend("カン".as_bytes());
        let mut file = vec![1, 0];
        file.extend(6u32.to_le_bytes());
        file.extend(&b);
        let shard = Shard::from_bytes(file, PathBuf::from("bad.loudstxt3")).unwrap();
        assert!(matches!(shard.slot(0), Err(DictError::Corrupt { .. })));
    }

    #[test]
    fn cc_row_default_sparse_and_missing() {
        // 既定 -3.5、key 7 = -1.0
        let mut b = Vec::new();
        b.extend((-1i32).to_le_bytes());
        b.extend((-3.5f32).to_le_bytes());
        b.extend(7i32.to_le_bytes());
        b.extend((-1.0f32).to_le_bytes());
        let row = parse_cc_row(&b).unwrap();
        assert_eq!(row.len(), CID_COUNT);
        assert_eq!(row[0], -3.5);
        assert_eq!(row[7], -1.0);
        assert_eq!(row[CID_COUNT - 1], -3.5);
        assert!(parse_cc_row(&[0, 0, 0, 0, 0, 0, 0, 0]).is_err());

        let d = dict();
        // 範囲外の行 / 列は -25
        assert_eq!(d.cc(CID_COUNT, 0), -25.0);
        assert_eq!(d.cc(0, CID_COUNT), -25.0);
        // 実データ: 行ファイルの既定値と疎な値に一致する
        let path = d.dir().join("cb/5.binary");
        let bytes = std::fs::read(path).unwrap();
        let expect = parse_cc_row(&bytes).unwrap();
        for latter in [0usize, 1, 5, 1288, CID_COUNT - 1] {
            assert_eq!(d.cc(5, latter).to_bits(), expect[latter].to_bits());
        }
    }

    #[test]
    fn mm_indexing_and_sentinel() {
        let d = dict();
        let bytes = std::fs::read(d.dir().join("mm.binary")).unwrap();
        let at = |f: usize, l: usize| f32::from_bits(le_u32(&bytes, (f * MID_COUNT + l) * 4));
        assert_eq!(d.mm(3, 7).to_bits(), at(3, 7).to_bits());
        assert_eq!(d.mm(7, 3).to_bits(), at(7, 3).to_bits());
        assert_eq!(d.mm(501, 501).to_bits(), at(501, 501).to_bits());
        assert_eq!(d.mm(500, 3), 0.0);
        assert_eq!(d.mm(3, 500), 0.0);
        assert_eq!(d.mm(502, 3), 0.0);
        assert_eq!(bytes.len(), (MID_COUNT * MID_COUNT + 1) * 4); // 末尾に余分な 1 float
    }

    #[test]
    fn prefix_entries_returns_every_matching_prefix() {
        let d = dict();
        let es = d.prefix_entries("キョウ").unwrap();
        assert!(es.iter().any(|e| e.ruby == "キョウ" && e.word == "今日"), "今日");
        assert!(es.iter().all(|e| "キョウ".starts_with(e.ruby.as_str())));
        let rubies: std::collections::BTreeSet<&str> = es.iter().map(|e| e.ruby.as_str()).collect();
        assert!(rubies.contains("キョウ"));
        // 未知の先頭文字・空文字は空
        assert!(d.prefix_entries("😀").unwrap().is_empty());
        assert!(d.prefix_entries("").unwrap().is_empty());
    }

    #[test]
    fn missing_dictionary_is_an_error_not_a_panic() {
        let r = Dictionary::open(Path::new("/nonexistent/novakey"));
        assert!(matches!(r, Err(DictError::NotFound(_))));
    }
}
