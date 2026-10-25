#![forbid(unsafe_code)]

//! 審判のモデル (`tools/gen_judge_model.py` が書く GGUF) を読むだけの最小の GGUF リーダー。
//! 開くときはヘッダ・メタデータ (中身は `general.alignment` 以外読み飛ばすが、型はすべて解釈してオフセットを保つ)・
//! テンソル情報だけを読む。テンソルのデータは呼ばれたときに 1 つずつ seek + read_exact で小さな塊に分けて渡すので、
//! ファイル全体 (41MB) を一度にメモリへ置かない。
//! 対応する版は 2 と 3 (どちらも個数・長さが u64 で同じ配置)、テンソルの型は F32 と Q8_0 だけ。
//! 壊れた / 対応外の入力は固定文字列の `Err` を返し、panic しない (オフセットと大きさは全部 checked_*)。

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

type R<T> = Result<T, &'static str>;

const MAGIC: &[u8; 4] = b"GGUF";
const DEFAULT_ALIGNMENT: u64 = 32;
/// ggml のテンソルの次元数の上限。
const MAX_DIMS: usize = 4;
/// 配列の入れ子の上限 (再帰でスタックを使い切らないように)。
const MAX_NESTING: u32 = 8;
/// ヘッダを読むときの読み込みバッファ。
const HEADER_BUF: usize = 64 * 1024;
/// テンソル名の長さの上限 (壊れた長さで大きな確保をしないように)。
const MAX_NAME: usize = 1024;

const GGML_F32: u32 = 0;
const GGML_Q8_0: u32 = 8;
/// q8_0 の 1 ブロック: f16 の尺度 + 32 個の i8。
const Q8_0_BLOCK_ELEMS: usize = 32;
pub const Q8_0_BLOCK_BYTES: usize = 34;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dtype {
    F32,
    Q8_0,
}

impl Dtype {
    fn from_ggml(t: u32) -> R<Self> {
        match t {
            GGML_F32 => Ok(Self::F32),
            GGML_Q8_0 => Ok(Self::Q8_0),
            _ => Err("unsupported gguf tensor type"),
        }
    }

    /// 要素数 `n` (最内の次元 `ne0`) のテンソルのバイト数。
    fn byte_len(self, n: usize, ne0: usize) -> R<usize> {
        match self {
            Self::F32 => n.checked_mul(4).ok_or("gguf tensor too large"),
            Self::Q8_0 => {
                if ne0 % Q8_0_BLOCK_ELEMS != 0 {
                    return Err("gguf q8_0 row not a multiple of 32");
                }
                (n / Q8_0_BLOCK_ELEMS).checked_mul(Q8_0_BLOCK_BYTES).ok_or("gguf tensor too large")
            }
        }
    }

    /// データを塊に分けるときの単位 (要素やブロックを塊の境目で割らない)。
    fn unit(self) -> usize {
        match self {
            Self::F32 => 4,
            Self::Q8_0 => Q8_0_BLOCK_BYTES,
        }
    }
}

/// 1 つのテンソルの情報。
#[derive(Clone, Debug)]
pub struct Tensor {
    /// 行優先 (遅い次元が先)。GGUF は速い次元を先に書くので逆順にして持つ。
    pub dims: Vec<usize>,
    pub dtype: Dtype,
    /// データのバイト数。
    pub len: usize,
    /// ファイルの中での開始位置。
    start: u64,
}

/// 開いた GGUF ファイル (テンソル情報だけを持ち、データは `read` で 1 つずつ読む)。
pub struct Gguf<S> {
    src: S,
    infos: Vec<(String, Tensor)>,
}

/// 読んだ位置と全体の長さを数えながら読む (長さは読む前に残りと比べ、壊れた長さで大きく確保しない)。
struct Header<S> {
    r: BufReader<S>,
    pos: u64,
    end: u64,
}

impl<S: Read> Header<S> {
    fn advance(&mut self, n: usize) -> R<()> {
        let n = u64::try_from(n).map_err(|_| "gguf truncated")?;
        let next = self.pos.checked_add(n).ok_or("gguf truncated")?;
        if next > self.end {
            return Err("gguf truncated");
        }
        self.pos = next;
        Ok(())
    }

    fn fill(&mut self, buf: &mut [u8]) -> R<()> {
        self.advance(buf.len())?;
        self.r.read_exact(buf).map_err(|_| "gguf truncated")
    }

    fn skip(&mut self, n: usize) -> R<()> {
        self.advance(n)?;
        let mut left = n;
        while left > 0 {
            let avail = self.r.fill_buf().map_err(|_| "gguf truncated")?.len();
            if avail == 0 {
                return Err("gguf truncated");
            }
            let k = avail.min(left);
            self.r.consume(k);
            left -= k;
        }
        Ok(())
    }

    fn u32(&mut self) -> R<u32> {
        let mut b = [0u8; 4];
        self.fill(&mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    fn len(&mut self) -> R<usize> {
        let mut b = [0u8; 8];
        self.fill(&mut b)?;
        usize::try_from(u64::from_le_bytes(b)).map_err(|_| "gguf length too large")
    }

    /// 長さ付きの文字列。`max` を超える長さは読まずに失敗する。
    fn string(&mut self, max: usize) -> R<Vec<u8>> {
        let n = self.len()?;
        if n > max {
            return Err("gguf string too long");
        }
        let mut v = vec![0u8; n];
        self.fill(&mut v)?;
        Ok(v)
    }

    fn skip_string(&mut self) -> R<()> {
        let n = self.len()?;
        self.skip(n)
    }

    /// 型 `t` の値を 1 つ読み飛ばす。
    fn skip_value(&mut self, t: u32, depth: u32) -> R<()> {
        match t {
            8 => self.skip_string(),
            9 => {
                if depth >= MAX_NESTING {
                    return Err("gguf metadata nested too deep");
                }
                let et = self.u32()?;
                let n = self.len()?;
                match fixed_size(et) {
                    Some(sz) => self.skip(n.checked_mul(sz).ok_or("gguf truncated")?),
                    // 要素ごとに最低 8 バイト読むので、壊れた個数でもファイルの長さで止まる。
                    None if et == 8 || et == 9 => (0..n).try_for_each(|_| self.skip_value(et, depth + 1)),
                    None => Err("unknown gguf metadata type"),
                }
            }
            _ => match fixed_size(t) {
                Some(sz) => self.skip(sz),
                None => Err("unknown gguf metadata type"),
            },
        }
    }
}

/// 固定長のメタデータ型の大きさ (文字列・配列・未知の型は None)。
fn fixed_size(t: u32) -> Option<usize> {
    match t {
        0 | 1 | 7 => Some(1),   // u8, i8, bool
        2 | 3 => Some(2),       // u16, i16
        4 | 5 | 6 => Some(4),   // u32, i32, f32
        10 | 11 | 12 => Some(8), // u64, i64, f64
        _ => None,
    }
}

impl Gguf<File> {
    /// ファイルを開いてヘッダとテンソル情報だけを読む。
    pub fn open(path: &Path) -> R<Self> {
        let file = File::open(path).map_err(|_| "cannot open model file")?;
        Self::from_reader(file)
    }
}

impl<S: Read + Seek> Gguf<S> {
    pub fn from_reader(mut src: S) -> R<Self> {
        let end = src.seek(SeekFrom::End(0)).map_err(|_| "cannot read model file")?;
        src.seek(SeekFrom::Start(0)).map_err(|_| "cannot read model file")?;
        let mut h = Header { r: BufReader::with_capacity(HEADER_BUF, &mut src), pos: 0, end };
        let mut magic = [0u8; 4];
        h.fill(&mut magic)?;
        if &magic != MAGIC {
            return Err("not a gguf file");
        }
        let version = h.u32()?;
        if version != 2 && version != 3 {
            return Err("unsupported gguf version");
        }
        let n_tensors = h.len()?;
        let n_kv = h.len()?;

        let mut alignment = DEFAULT_ALIGNMENT;
        for _ in 0..n_kv {
            let key_len = h.len()?;
            let is_alignment = if key_len == b"general.alignment".len() {
                let mut k = [0u8; 17];
                h.fill(&mut k)?;
                &k == b"general.alignment"
            } else {
                h.skip(key_len)?;
                false
            };
            let t = h.u32()?;
            if is_alignment {
                if t != 4 {
                    return Err("gguf alignment is not a u32");
                }
                let a = u64::from(h.u32()?);
                if a == 0 || !a.is_power_of_two() {
                    return Err("bad gguf alignment");
                }
                alignment = a;
            } else {
                h.skip_value(t, 0)?;
            }
        }

        // (名前, テンソル, データ部の中のオフセット)
        let mut raw = Vec::new();
        for _ in 0..n_tensors {
            let name = String::from_utf8(h.string(MAX_NAME)?).map_err(|_| "gguf tensor name not utf-8")?;
            let nd = usize::try_from(h.u32()?).map_err(|_| "too many gguf dims")?;
            if nd == 0 || nd > MAX_DIMS {
                return Err("unsupported gguf tensor rank");
            }
            let mut dims = Vec::with_capacity(nd);
            for _ in 0..nd {
                dims.push(h.len()?);
            }
            let ne0 = dims[0];
            dims.reverse();
            let dtype = Dtype::from_ggml(h.u32()?)?;
            let mut ob = [0u8; 8];
            h.fill(&mut ob)?;
            let off = u64::from_le_bytes(ob);
            if off % alignment != 0 {
                return Err("misaligned gguf tensor");
            }
            let n = dims.iter().try_fold(1usize, |a, &d| a.checked_mul(d)).ok_or("gguf tensor too large")?;
            let len = dtype.byte_len(n, ne0)?;
            raw.push((name, Tensor { dims, dtype, len, start: 0 }, off));
        }

        let data_start = h.pos.checked_next_multiple_of(alignment).ok_or("gguf truncated")?;
        drop(h);
        let mut infos: Vec<(String, Tensor)> = Vec::with_capacity(raw.len());
        for (name, mut t, off) in raw {
            let start = data_start.checked_add(off).ok_or("gguf truncated")?;
            let len = u64::try_from(t.len).map_err(|_| "gguf truncated")?;
            if start.checked_add(len).ok_or("gguf truncated")? > end {
                return Err("gguf truncated");
            }
            if infos.iter().any(|(n, _)| *n == name) {
                return Err("duplicate gguf tensor name");
            }
            t.start = start;
            infos.push((name, t));
        }
        Ok(Self { src, infos })
    }

    pub fn tensor(&self, name: &str) -> R<Tensor> {
        self.infos.iter().find(|(n, _)| n == name).map(|(_, t)| t.clone()).ok_or("missing gguf tensor")
    }

    /// テンソル `t` のデータを先頭から、`chunk` バイト前後 (要素・ブロックの境目で切る) の塊に分けて `f` に渡す。
    pub fn read<F: FnMut(&[u8]) -> R<()>>(&mut self, t: &Tensor, chunk: usize, mut f: F) -> R<()> {
        let unit = t.dtype.unit();
        let step = (chunk / unit).max(1).checked_mul(unit).ok_or("gguf tensor too large")?;
        self.src.seek(SeekFrom::Start(t.start)).map_err(|_| "cannot read model file")?;
        let mut buf = vec![0u8; step.min(t.len)];
        let mut left = t.len;
        while left > 0 {
            let k = step.min(left);
            let b = &mut buf[..k];
            self.src.read_exact(b).map_err(|_| "cannot read model file")?;
            f(b)?;
            left -= k;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn s(b: &mut Vec<u8>, v: &str) {
        b.extend_from_slice(&(v.len() as u64).to_le_bytes());
        b.extend_from_slice(v.as_bytes());
    }

    /// F32 [2, 3] と Q8_0 [1, 32] の 2 つ、メタデータに全種類の型を並べた小さな GGUF。
    fn tiny(version: u32, q8_type: u32) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"GGUF");
        b.extend_from_slice(&version.to_le_bytes());
        b.extend_from_slice(&2u64.to_le_bytes());
        b.extend_from_slice(&5u64.to_le_bytes());
        // kv: u8, string, array of strings, array of u16, alignment
        s(&mut b, "a.u8");
        b.extend_from_slice(&0u32.to_le_bytes());
        b.push(7);
        s(&mut b, "a.str");
        b.extend_from_slice(&8u32.to_le_bytes());
        s(&mut b, "hello");
        s(&mut b, "a.strs");
        b.extend_from_slice(&9u32.to_le_bytes());
        b.extend_from_slice(&8u32.to_le_bytes());
        b.extend_from_slice(&2u64.to_le_bytes());
        s(&mut b, "x");
        s(&mut b, "yz");
        s(&mut b, "a.u16s");
        b.extend_from_slice(&9u32.to_le_bytes());
        b.extend_from_slice(&2u32.to_le_bytes());
        b.extend_from_slice(&3u64.to_le_bytes());
        b.extend_from_slice(&[1, 0, 2, 0, 3, 0]);
        s(&mut b, "general.alignment");
        b.extend_from_slice(&4u32.to_le_bytes());
        b.extend_from_slice(&64u32.to_le_bytes());
        // tensor infos
        s(&mut b, "f");
        b.extend_from_slice(&2u32.to_le_bytes());
        b.extend_from_slice(&3u64.to_le_bytes());
        b.extend_from_slice(&2u64.to_le_bytes());
        b.extend_from_slice(&GGML_F32.to_le_bytes());
        b.extend_from_slice(&0u64.to_le_bytes());
        s(&mut b, "q");
        b.extend_from_slice(&2u32.to_le_bytes());
        b.extend_from_slice(&32u64.to_le_bytes());
        b.extend_from_slice(&1u64.to_le_bytes());
        b.extend_from_slice(&q8_type.to_le_bytes());
        b.extend_from_slice(&64u64.to_le_bytes());
        while b.len() % 64 != 0 {
            b.push(0);
        }
        for v in [1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.resize(b.len() + 40, 0); // 24 バイトの f32 を 64 に揃える
        b.extend((0..34u8).map(|i| i + 1));
        b
    }

    fn parse(b: Vec<u8>) -> R<Gguf<Cursor<Vec<u8>>>> {
        Gguf::from_reader(Cursor::new(b))
    }

    /// テンソルのデータを `chunk` バイトの塊で読み、塊ごとの長さとつないだデータを返す。
    fn read_all(g: &mut Gguf<Cursor<Vec<u8>>>, name: &str, chunk: usize) -> (Vec<usize>, Vec<u8>) {
        let t = g.tensor(name).unwrap();
        let (mut sizes, mut out) = (Vec::new(), Vec::new());
        g.read(&t, chunk, |b| {
            sizes.push(b.len());
            out.extend_from_slice(b);
            Ok(())
        })
        .unwrap();
        (sizes, out)
    }

    #[test]
    fn 小さなgguf_のテンソルの形と型とデータが読める() {
        for version in [2, 3] {
            let mut g = parse(tiny(version, GGML_Q8_0)).unwrap();
            let f = g.tensor("f").unwrap();
            assert_eq!((f.dims.as_slice(), f.dtype, f.len), (&[2usize, 3][..], Dtype::F32, 24));
            let (_, data) = read_all(&mut g, "f", 1 << 16);
            let vals: Vec<f32> = data.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
            assert_eq!(vals, [1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
            let q = g.tensor("q").unwrap();
            assert_eq!((q.dims.as_slice(), q.dtype), (&[1usize, 32][..], Dtype::Q8_0));
            assert_eq!(read_all(&mut g, "q", 1 << 16).1, (1..=34u8).collect::<Vec<_>>());
            assert_eq!(g.tensor("nope").err(), Some("missing gguf tensor"));
        }
    }

    #[test]
    fn データは要素の境目で塊に分けて渡される() {
        let mut g = parse(tiny(3, GGML_Q8_0)).unwrap();
        // 10 バイトを頼んでも f32 は 4 の倍数 (8) で切り、最後は残り
        let (sizes, data) = read_all(&mut g, "f", 10);
        assert_eq!(sizes, [8, 8, 8]);
        assert_eq!(data.len(), 24);
        // q8_0 は 1 ブロック (34) より小さく頼んでも 1 ブロックずつ
        assert_eq!(read_all(&mut g, "q", 1).0, [34]);
    }

    #[test]
    fn 途中で切れたファイルはどこで切れても失敗し_panic_しない() {
        let full = tiny(3, GGML_Q8_0);
        for n in 0..full.len() {
            assert!(parse(full[..n].to_vec()).is_err(), "len {n}");
        }
    }

    #[test]
    fn 先頭の印が違うと失敗する() {
        let mut b = tiny(3, GGML_Q8_0);
        b[0] = b'X';
        assert_eq!(parse(b).err(), Some("not a gguf file"));
    }

    #[test]
    fn 対応していない版は失敗する() {
        for v in [1, 4] {
            assert_eq!(parse(tiny(v, GGML_Q8_0)).err(), Some("unsupported gguf version"));
        }
    }

    #[test]
    fn 対応していないテンソルの型は失敗する() {
        // 1 = F16, 2 = Q4_0
        for t in [1, 2] {
            assert_eq!(parse(tiny(3, t)).err(), Some("unsupported gguf tensor type"));
        }
    }

    #[test]
    fn 巨大な個数や長さは失敗し_panic_しない() {
        let mut b = tiny(3, GGML_Q8_0);
        b[8..16].copy_from_slice(&u64::MAX.to_le_bytes()); // tensor count
        assert!(parse(b).is_err());
        let mut b = tiny(3, GGML_Q8_0);
        b[24..32].copy_from_slice(&u64::MAX.to_le_bytes()); // 最初のキーの長さ
        assert!(parse(b).is_err());
    }
}
