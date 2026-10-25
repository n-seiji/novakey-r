#![forbid(unsafe_code)]

//! 審判のモデル (`tools/gen_judge_model.py` が書く GGUF) を読むだけの最小の GGUF リーダー。
//! ヘッダ・メタデータ (中身は `general.alignment` 以外読み飛ばすが、型はすべて解釈してオフセットを保つ)・
//! テンソル情報を読み、テンソルのデータはファイル全体を持つ `Vec<u8>` の切り出しとして返す。
//! 対応する版は 2 と 3 (どちらも個数・長さが u64 で同じ配置)、テンソルの型は F32 と Q8_0 だけ。
//! 壊れた / 対応外の入力は固定文字列の `Err` を返し、panic しない (オフセットと大きさは全部 checked_*)。

use std::path::Path;

type R<T> = Result<T, &'static str>;

const MAGIC: &[u8; 4] = b"GGUF";
const DEFAULT_ALIGNMENT: usize = 32;
/// ggml のテンソルの次元数の上限。
const MAX_DIMS: usize = 4;
/// 配列の入れ子の上限 (再帰でスタックを使い切らないように)。
const MAX_NESTING: u32 = 8;

const GGML_F32: u32 = 0;
const GGML_Q8_0: u32 = 8;
/// q8_0 の 1 ブロック: f16 の尺度 + 32 個の i8。
const Q8_0_BLOCK_ELEMS: usize = 32;
const Q8_0_BLOCK_BYTES: usize = 34;

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
}

struct Info {
    name: String,
    /// 行優先 (遅い次元が先)。GGUF は速い次元を先に書くので逆順にして持つ。
    dims: Vec<usize>,
    dtype: Dtype,
    /// ファイル全体の中での開始位置と長さ。
    start: usize,
    len: usize,
}

/// 読み込んだ GGUF ファイル。
pub struct Gguf {
    bytes: Vec<u8>,
    infos: Vec<Info>,
}

/// 1 つのテンソル (データはファイルの切り出し)。
pub struct Tensor<'a> {
    /// 行優先 (遅い次元が先)。
    pub dims: &'a [usize],
    pub dtype: Dtype,
    pub data: &'a [u8],
}

struct Cursor<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> R<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or("gguf truncated")?;
        let s = self.b.get(self.pos..end).ok_or("gguf truncated")?;
        self.pos = end;
        Ok(s)
    }

    fn skip(&mut self, n: usize) -> R<()> {
        self.take(n).map(|_| ())
    }

    fn u32(&mut self) -> R<u32> {
        let s = self.take(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }

    fn u64(&mut self) -> R<u64> {
        let s = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(s);
        Ok(u64::from_le_bytes(a))
    }

    fn len(&mut self) -> R<usize> {
        usize::try_from(self.u64()?).map_err(|_| "gguf length too large")
    }

    fn string(&mut self) -> R<&'a [u8]> {
        let n = self.len()?;
        self.take(n)
    }

    /// 型 `t` の値を 1 つ読み飛ばす。
    fn skip_value(&mut self, t: u32, depth: u32) -> R<()> {
        match t {
            0 | 1 | 7 => self.skip(1),  // u8, i8, bool
            2 | 3 => self.skip(2),      // u16, i16
            4 | 5 | 6 => self.skip(4),  // u32, i32, f32
            10 | 11 | 12 => self.skip(8), // u64, i64, f64
            8 => self.string().map(|_| ()),
            9 => {
                if depth >= MAX_NESTING {
                    return Err("gguf metadata nested too deep");
                }
                let et = self.u32()?;
                let n = self.len()?;
                let fixed = match et {
                    0 | 1 | 7 => Some(1usize),
                    2 | 3 => Some(2),
                    4 | 5 | 6 => Some(4),
                    10 | 11 | 12 => Some(8),
                    8 | 9 => None,
                    _ => return Err("unknown gguf metadata type"),
                };
                match fixed {
                    Some(sz) => self.skip(n.checked_mul(sz).ok_or("gguf truncated")?),
                    // 要素ごとに最低 8 バイト読むので、壊れた個数でもファイルの長さで止まる。
                    None => (0..n).try_for_each(|_| self.skip_value(et, depth + 1)),
                }
            }
            _ => Err("unknown gguf metadata type"),
        }
    }
}

impl Gguf {
    /// ファイルを丸ごと読んで解釈する。
    pub fn read(path: &Path) -> R<Self> {
        let bytes = std::fs::read(path).map_err(|_| "cannot read model file")?;
        Self::parse(bytes)
    }

    pub fn parse(bytes: Vec<u8>) -> R<Self> {
        let mut c = Cursor { b: &bytes, pos: 0 };
        if c.take(4)? != MAGIC {
            return Err("not a gguf file");
        }
        let version = c.u32()?;
        if version != 2 && version != 3 {
            return Err("unsupported gguf version");
        }
        let n_tensors = c.len()?;
        let n_kv = c.len()?;

        let mut alignment = DEFAULT_ALIGNMENT;
        for _ in 0..n_kv {
            let key = c.string()?;
            let t = c.u32()?;
            if key == b"general.alignment" {
                if t != 4 {
                    return Err("gguf alignment is not a u32");
                }
                let a = usize::try_from(c.u32()?).map_err(|_| "bad gguf alignment")?;
                if a == 0 || !a.is_power_of_two() {
                    return Err("bad gguf alignment");
                }
                alignment = a;
            } else {
                c.skip_value(t, 0)?;
            }
        }

        // (名前, 行優先の次元, 型, データ部の中のオフセット, バイト数)
        let mut raw = Vec::new();
        for _ in 0..n_tensors {
            let name = std::str::from_utf8(c.string()?).map_err(|_| "gguf tensor name not utf-8")?.to_owned();
            let nd = usize::try_from(c.u32()?).map_err(|_| "too many gguf dims")?;
            if nd == 0 || nd > MAX_DIMS {
                return Err("unsupported gguf tensor rank");
            }
            let mut dims = Vec::with_capacity(nd);
            for _ in 0..nd {
                dims.push(c.len()?);
            }
            let ne0 = dims[0];
            dims.reverse();
            let dtype = Dtype::from_ggml(c.u32()?)?;
            let off = c.len()?;
            if off % alignment != 0 {
                return Err("misaligned gguf tensor");
            }
            let n = dims.iter().try_fold(1usize, |a, &d| a.checked_mul(d)).ok_or("gguf tensor too large")?;
            let len = dtype.byte_len(n, ne0)?;
            raw.push((name, dims, dtype, off, len));
        }

        let data_start = c.pos.checked_next_multiple_of(alignment).ok_or("gguf truncated")?;
        let mut infos: Vec<Info> = Vec::with_capacity(raw.len());
        for (name, dims, dtype, off, len) in raw {
            let start = data_start.checked_add(off).ok_or("gguf truncated")?;
            let end = start.checked_add(len).ok_or("gguf truncated")?;
            if end > bytes.len() {
                return Err("gguf truncated");
            }
            if infos.iter().any(|i| i.name == name) {
                return Err("duplicate gguf tensor name");
            }
            infos.push(Info { name, dims, dtype, start, len });
        }
        Ok(Self { bytes, infos })
    }

    pub fn tensor(&self, name: &str) -> R<Tensor<'_>> {
        let i = self.infos.iter().find(|i| i.name == name).ok_or("missing gguf tensor")?;
        // parse で範囲を確かめてあるが、ここでも get で読む (panic しない)。
        let end = i.start.checked_add(i.len).ok_or("gguf truncated")?;
        let data = self.bytes.get(i.start..end).ok_or("gguf truncated")?;
        Ok(Tensor { dims: &i.dims, dtype: i.dtype, data })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn 小さなgguf_のテンソルの形と型とデータが読める() {
        for version in [2, 3] {
            let g = Gguf::parse(tiny(version, GGML_Q8_0)).unwrap();
            let f = g.tensor("f").unwrap();
            assert_eq!((f.dims, f.dtype), (&[2usize, 3][..], Dtype::F32));
            let vals: Vec<f32> = f.data.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
            assert_eq!(vals, [1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
            let q = g.tensor("q").unwrap();
            assert_eq!((q.dims, q.dtype), (&[1usize, 32][..], Dtype::Q8_0));
            assert_eq!(q.data, (1..=34u8).collect::<Vec<_>>().as_slice());
            assert_eq!(g.tensor("nope").err(), Some("missing gguf tensor"));
        }
    }

    #[test]
    fn 途中で切れたファイルはどこで切れても失敗し_panic_しない() {
        let full = tiny(3, GGML_Q8_0);
        for n in 0..full.len() {
            assert!(Gguf::parse(full[..n].to_vec()).is_err(), "len {n}");
        }
    }

    #[test]
    fn 先頭の印が違うと失敗する() {
        let mut b = tiny(3, GGML_Q8_0);
        b[0] = b'X';
        assert_eq!(Gguf::parse(b).err(), Some("not a gguf file"));
    }

    #[test]
    fn 対応していない版は失敗する() {
        for v in [1, 4] {
            assert_eq!(Gguf::parse(tiny(v, GGML_Q8_0)).err(), Some("unsupported gguf version"));
        }
    }

    #[test]
    fn 対応していないテンソルの型は失敗する() {
        // 1 = F16, 2 = Q4_0
        for t in [1, 2] {
            assert_eq!(Gguf::parse(tiny(3, t)).err(), Some("unsupported gguf tensor type"));
        }
    }

    #[test]
    fn 巨大な個数や長さは失敗し_panic_しない() {
        let mut b = tiny(3, GGML_Q8_0);
        b[8..16].copy_from_slice(&u64::MAX.to_le_bytes()); // tensor count
        assert!(Gguf::parse(b).is_err());
        let mut b = tiny(3, GGML_Q8_0);
        b[24..32].copy_from_slice(&u64::MAX.to_le_bytes()); // 最初のキーの長さ
        assert!(Gguf::parse(b).is_err());
    }
}
