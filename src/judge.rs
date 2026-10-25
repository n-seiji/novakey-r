#![forbid(unsafe_code)]

//! 変換の全文候補を、端末内の小さな言語モデル (rinna/japanese-gpt2-xsmall, GPT-2 6 層) の
//! 対数尤度で並べ替える審判。重みは q8_0 のブロックのままメモリに置き、
//! 層ごとに f32 の作業領域へ戻して Accelerate の sgemm に流す (f32 の全コピーは持たない)。
//! 出力射影 (= 埋め込み) は語彙を塊に分けて、塊ごとに戻す -> sgemm -> log-sum-exp を更新する。
//! モデルは最初に使うときに 1 度だけ読む。読めなければ 1 度だけログして engine の順のまま。
//! 入力文字列はログにもトレースにも出さない。

use std::cell::{Cell, RefCell};
use std::io::{Read, Seek};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use rayon::prelude::*;
use tokenizers::Tokenizer;

use crate::gguf;

const NL: usize = 6;
const NH: usize = 8;
const DM: usize = 512;
/// MLP の中間幅 (rinna/japanese-gpt2-xsmall は 4.5 倍の 2304)。
const FF: usize = 2304;
const EPS: f64 = 1e-5;
const BOS: u32 = 1;
const EOS: u32 = 2;
/// engine のスコアに足す審判の重み。
const WEIGHT: f64 = 1.0;
/// これより長い系列 (トークン数) は審判にかけない。
const MAX_TOKENS: usize = 200;
/// CPU の並列に使うスレッド数 (IME が全コアを占有しないように)。
const THREADS: usize = 3;
/// 重みを f32 に戻す作業領域の大きさ (要素数)。層の重みはこの大きさの塊に分けて流す。
const SCRATCH_ELEMS: usize = 1 << 19;
/// 出力射影の 1 塊あたりの語彙数 (sgemm の経路)。
const VOCAB_CHUNK: usize = 1024;
/// これ以下の行数なら入力を量子化して整数の内積で流す (超えると sgemm)。
const SMALL_M: usize = 12;
/// 出力射影でも、隠れ状態の行数がこれ以下なら整数の内積。
const SMALL_R: usize = 12;
/// 重みを f32 に戻すとき、これ以上の行数ならスレッドに分ける。
const PAR_DEQUANT_ROWS: usize = 64;
/// 整数の内積の経路で、1 タスクが受け持つ重みの行数 / 語彙数。
const SMALL_CHUNK: usize = 64;
const VOCAB_SMALL_CHUNK: usize = 512;
/// 注意をスレッドに分けるのに必要な行数。
const PAR_ATTN_ROWS: usize = 24;
/// 木に残すノード数の上限 (1 ノード約 26KB)。
const MAX_NODES: usize = 640;
/// モデルファイルからテンソルのデータを読むときの 1 回の読み込みの大きさ (バイト)。
const READ_CHUNK: usize = 1 << 20;

/// エラーは固定文字列だけ (tokenizers のエラー文はログに出さない)。
type R<T> = Result<T, &'static str>;

/// 元のエラー (tokenizers / io) は捨てて、場所を表す固定文字列にする。
fn fail<E>(msg: &'static str) -> impl Fn(E) -> &'static str {
    move |_| msg
}

fn f16_to_f32(h: u16) -> f32 {
    let sign = u32::from(h >> 15) << 31;
    let exp = u32::from((h >> 10) & 0x1f);
    let man = u32::from(h & 0x3ff);
    match exp {
        0 => {
            let v = man as f32 / 16_777_216.0;
            if sign == 0 { v } else { -v }
        }
        31 => f32::from_bits(sign | 0x7f80_0000 | (man << 13)),
        _ => f32::from_bits(sign | ((exp + 112) << 23) | (man << 13)),
    }
}

#[inline(never)]
fn dq8(q: &[u8], scales: &[f32], out: &mut [f32]) {
    for ((ob, qb), &d) in out.chunks_exact_mut(32).zip(q.chunks_exact(32)).zip(scales) {
        for (o, &v) in ob.iter_mut().zip(qb) {
            *o = f32::from(v as i8) * d;
        }
    }
}

/// q8_0 のまま持つ行列 [rows, cols] (行ごとに cols が連続)。
struct QMat {
    rows: usize,
    cols: usize,
    qs: Vec<u8>,
    scales: Vec<f32>,
}

impl QMat {
    /// テンソル `name` を塊ごとに読みながら、尺度と量子化値に分けて持つ (ファイルのデータ全体は持たない)。
    fn from_gguf<S: Read + Seek>(g: &mut gguf::Gguf<S>, name: &str) -> R<Self> {
        let t = g.tensor(name)?;
        let &[rows, cols] = t.dims.as_slice() else {
            return Err("tensor is not a matrix");
        };
        if cols % 32 != 0 {
            return Err("cols not a multiple of 32");
        }
        let nb = rows.checked_mul(cols).ok_or("tensor too large")? / 32;
        if t.dtype != gguf::Dtype::Q8_0 {
            return Err("unsupported tensor dtype");
        }
        let bsz = gguf::Q8_0_BLOCK_BYTES;
        if nb.checked_mul(bsz) != Some(t.len) {
            return Err("unexpected tensor size");
        }
        let qlen = bsz - 2;
        let mut scales = Vec::with_capacity(nb);
        let mut qs = Vec::with_capacity(nb * qlen);
        g.read(&t, READ_CHUNK, |data| {
            for blk in data.chunks_exact(bsz) {
                scales.push(f16_to_f32(u16::from_le_bytes([blk[0], blk[1]])));
                qs.extend_from_slice(&blk[2..]);
            }
            Ok(())
        })?;
        Ok(Self { rows, cols, qs, scales })
    }

    /// 行 `r0..r1` を f32 に戻して `out` ([r1 - r0, cols]) に書く (行数が多ければスレッドに分ける)。
    fn dequant_rows(&self, r0: usize, r1: usize, out: &mut [f32]) {
        let bpr = self.cols / 32; // 1 行のブロック数
        let n_rows = r1 - r0;
        let one = |ri: usize, orow: &mut [f32]| {
            let b0 = (r0 + ri) * bpr;
            let scales = &self.scales[b0..b0 + bpr];
            dq8(&self.qs[b0 * 32..(b0 + bpr) * 32], scales, orow);
        };
        let out = &mut out[..n_rows * self.cols];
        if n_rows >= PAR_DEQUANT_ROWS {
            out.par_chunks_mut(self.cols).enumerate().for_each(|(ri, orow)| one(ri, orow));
        } else {
            out.chunks_exact_mut(self.cols).enumerate().for_each(|(ri, orow)| one(ri, orow));
        }
    }

    /// 行を `chunk` 行ずつ作業領域へ f32 に戻しながら `f(先頭行, 末尾行 + 1, 戻した行)` を順に呼ぶ。
    fn stream_rows<F: FnMut(usize, usize, &[f32])>(&self, chunk: usize, scratch: &mut [f32], mut f: F) {
        let chunk = chunk.min(scratch.len() / self.cols).max(1);
        let mut r0 = 0;
        while r0 < self.rows {
            let r1 = (r0 + chunk).min(self.rows);
            self.dequant_rows(r0, r1, scratch);
            f(r0, r1, scratch);
            r0 = r1;
        }
    }

    /// `y[m, rows] = x[m, cols] * self^T` (`ldy` は y の行の幅)。
    fn matmul_t(&self, x: &[f32], m: usize, y: &mut [f32], ldy: usize, scratch: &mut [f32]) {
        let cols = self.cols;
        self.stream_rows(usize::MAX, scratch, |r0, r1, w| {
            crate::platform::sgemm_abt(m, r1 - r0, cols, x, cols, w, cols, &mut y[r0..], ldy);
        });
    }
}

/// 小さな行列積用に q8_0 に量子化した入力。q8_0 1 回だけだと活性化の外れ値で対数確率が 0.3 ほどずれるので、
/// 量子化の残りをもう 1 度 q8_0 にして「上位 m 行 + 残り m 行」の 2m 行にする
/// (内積は 2 つの和。約 14 bit の精度で、sgemm との差は 1e-3 未満)。
struct QRows {
    qs: Vec<i8>,
    d: Vec<f32>,
}

fn quantize_rows(x: &[f32], m: usize, cols: usize) -> QRows {
    let nb = m * cols / 32;
    let mut qs = vec![0i8; 2 * m * cols];
    let mut d = vec![0f32; 2 * nb];
    let (qh, ql) = qs.split_at_mut(m * cols);
    let (dh, dl) = d.split_at_mut(nb);
    for (bi, src) in x[..m * cols].chunks_exact(32).enumerate() {
        let amax = src.iter().fold(0f32, |a, &v| a.max(v.abs()));
        dh[bi] = amax / 127.0;
        if amax == 0.0 {
            continue;
        }
        let inv = 127.0 / amax;
        let hi = &mut qh[bi * 32..(bi + 1) * 32];
        for (h, &v) in hi.iter_mut().zip(src) {
            *h = (v * inv).round() as i8;
        }
        dl[bi] = amax / 127.0 / 254.0;
        for ((l, &h), &v) in ql[bi * 32..(bi + 1) * 32].iter_mut().zip(hi.iter()).zip(src) {
            *l = ((v * inv - f32::from(h)) * 254.0).round() as i8;
        }
    }
    QRows { qs, d }
}

impl QMat {
    /// 重みの行 `r0..r1` と、量子化した入力 `m` 行の内積を `yt[(r - r0) * m + i]` に書く (q8_0 のみ)。
    fn dot_rows(&self, r0: usize, r1: usize, x: &QRows, m: usize, yt: &mut [f32]) {
        let bpr = self.cols / 32;
        crate::platform::q8_dot_rows(&self.qs[r0 * bpr * 32..r1 * bpr * 32], &self.scales[r0 * bpr..r1 * bpr], &x.qs, &x.d, bpr, m, yt);
    }

    /// `y[m, rows] = x[m, cols] * self^T` を、入力を q8_0 にして整数の内積で求める (行数が少ないとき用)。
    fn matmul_small(&self, x: &[f32], m: usize, y: &mut [f32]) {
        let xq = quantize_rows(x, m, self.cols);
        let m2 = 2 * m;
        let mut yt = vec![0f32; self.rows * m2];
        yt.par_chunks_mut(SMALL_CHUNK * m2).enumerate().for_each(|(ci, out)| {
            let r0 = ci * SMALL_CHUNK;
            self.dot_rows(r0, r0 + out.len() / m2, &xq, m2, out);
        });
        for (r, col) in yt.chunks_exact(m2).enumerate() {
            for i in 0..m {
                y[i * self.rows + r] = col[i] + col[m + i];
            }
        }
    }
}

struct Linear {
    w: QMat,
    b: Vec<f32>,
}

impl Linear {
    /// `x[m, cols]` -> `[m, rows]` (バイアス込み)。行数が少なければ整数の内積、多ければ f32 に戻して sgemm。
    fn fwd(&self, x: &[f32], m: usize, scratch: &mut [f32]) -> Vec<f32> {
        let o = self.w.rows;
        let mut y = vec![0f32; m * o];
        if m <= SMALL_M {
            self.w.matmul_small(x, m, &mut y);
        } else {
            self.w.matmul_t(x, m, &mut y, o, scratch);
        }
        for row in y.chunks_exact_mut(o) {
            for (v, &bias) in row.iter_mut().zip(&self.b) {
                *v += bias;
            }
        }
        y
    }
}

struct Ln {
    w: Vec<f32>,
    b: Vec<f32>,
}

impl Ln {
    fn fwd(&self, x: &[f32]) -> Vec<f32> {
        let mut out = vec![0f32; x.len()];
        for (src, dst) in x.chunks_exact(DM).zip(out.chunks_exact_mut(DM)) {
            let mean = src.iter().sum::<f32>() / DM as f32;
            let var = src.iter().map(|&v| (v - mean) * (v - mean)).sum::<f32>() / DM as f32;
            let inv = 1.0 / (var + EPS as f32).sqrt();
            for (((o, &v), &w), &b) in dst.iter_mut().zip(src).zip(&self.w).zip(&self.b) {
                *o = (v - mean) * inv * w + b;
            }
        }
        out
    }
}

fn gelu_new(v: f32) -> f32 {
    0.5 * v * (1.0 + (0.797_884_6 * v * (1.0 + 0.044_715 * v * v)).tanh())
}

struct Block {
    ln1: Ln,
    ln2: Ln,
    attn: Linear,
    proj: Linear,
    fc: Linear,
    out: Linear,
}

/// 木の 1 ノード = 1 トークン (根は BOS)。位置は深さ。層ごとの K / V と、最終層の隠れ状態を持つ。
struct Node {
    parent: u32,
    tok: u32,
    depth: u32,
    /// このノードを最後に使った呼び出しの番号。
    stamp: u64,
    children: Vec<(u32, u32)>,
    /// `[層][K, V][DM]` に続けて隠れ状態 `[DM]`。空なら未計算。
    kv: Vec<f32>,
    /// 親の文脈でこのトークンが出る対数確率 (根は 0)。
    lp: f32,
    /// このノードの後に EOS が出る対数確率 (未計算は NaN)。
    eos_lp: f32,
}

const KV_LEN: usize = NL * 2 * DM + DM;

/// 過去に採点した系列の木 (共通の接頭辞の K / V と対数確率を使い回す)。
struct Cache {
    nodes: Vec<Node>,
    free: Vec<u32>,
    stamp: u64,
}

impl Cache {
    fn new() -> Self {
        Self { nodes: Vec::new(), free: Vec::new(), stamp: 0 }
    }

    fn live(&self) -> usize {
        self.nodes.len() - self.free.len()
    }

    fn alloc(&mut self, node: Node) -> u32 {
        if let Some(id) = self.free.pop() {
            self.nodes[id as usize] = node;
            id
        } else {
            self.nodes.push(node);
            (self.nodes.len() - 1) as u32
        }
    }

    /// `parent` の子 `tok` を返す。無ければ作って `pending` に積む。
    fn child(&mut self, parent: u32, tok: u32, pending: &mut Vec<u32>) -> u32 {
        let stamp = self.stamp;
        let found = self.nodes[parent as usize].children.iter().find(|c| c.0 == tok).map(|c| c.1);
        let id = match found {
            Some(id) => id,
            None => {
                let depth = self.nodes[parent as usize].depth + 1;
                let id = self.alloc(Node {
                    parent,
                    tok,
                    depth,
                    stamp,
                    children: Vec::new(),
                    kv: Vec::new(),
                    lp: f32::NAN,
                    eos_lp: f32::NAN,
                });
                self.nodes[parent as usize].children.push((tok, id));
                pending.push(id);
                id
            }
        };
        self.nodes[id as usize].stamp = stamp;
        id
    }

    /// 根から `id` までの添字 (根が先頭)。
    fn path(&self, id: u32) -> Vec<u32> {
        let mut p = Vec::with_capacity(self.nodes[id as usize].depth as usize + 1);
        let mut cur = id;
        loop {
            p.push(cur);
            if cur == 0 {
                break;
            }
            cur = self.nodes[cur as usize].parent;
        }
        p.reverse();
        p
    }

    /// 今回使っていない古い葉から捨てて、ノード数を `MAX_NODES` に収める (根と今回のノードは残す)。
    fn evict(&mut self) {
        while self.live() > MAX_NODES {
            let mut leaves: Vec<(u64, u32)> = (1..self.nodes.len() as u32)
                .filter(|&i| {
                    let n = &self.nodes[i as usize];
                    !n.kv.is_empty() && n.children.is_empty() && n.stamp < self.stamp
                })
                .map(|i| (self.nodes[i as usize].stamp, i))
                .collect();
            if leaves.is_empty() {
                return;
            }
            leaves.sort_unstable();
            for (_, id) in leaves {
                if self.live() <= MAX_NODES {
                    break;
                }
                let parent = self.nodes[id as usize].parent;
                self.nodes[parent as usize].children.retain(|c| c.1 != id);
                self.nodes[id as usize] = Node {
                    parent: 0,
                    tok: 0,
                    depth: 0,
                    stamp: 0,
                    children: Vec::new(),
                    kv: Vec::new(),
                    lp: f32::NAN,
                    eos_lp: f32::NAN,
                };
                self.free.push(id);
            }
        }
    }
}

/// 呼び出しをまたいで使い回す作業領域と木。
struct Work {
    /// 重みを f32 に戻す作業領域。
    scratch: Vec<f32>,
    cache: Cache,
}

struct Model {
    wte: QMat, // [V, d]
    wpe: Vec<f32>,
    blocks: Vec<Block>,
    lnf: Ln,
    work: Mutex<Work>,
}

impl Model {
    fn load(path: &Path) -> R<Self> {
        let file = RefCell::new(gguf::Gguf::open(path)?);
        let get_v = |k: &str| -> R<Vec<f32>> {
            let mut g = file.borrow_mut();
            let t = g.tensor(k)?;
            if t.dtype != gguf::Dtype::F32 {
                return Err("unreadable vector tensor");
            }
            let mut v = Vec::with_capacity(t.len / 4);
            g.read(&t, READ_CHUNK, |data| {
                v.extend(data.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])));
                Ok(())
            })?;
            Ok(v)
        };
        let ln = |p: &str, n: &str| -> R<Ln> {
            Ok(Ln { w: get_v(&format!("{p}.{n}.weight"))?, b: get_v(&format!("{p}.{n}.bias"))? })
        };
        let lin = |p: &str, n: &str| -> R<Linear> {
            let w = QMat::from_gguf(&mut file.borrow_mut(), &format!("{p}.{n}.weight"))?;
            Ok(Linear { w, b: get_v(&format!("{p}.{n}.bias"))? })
        };
        let mut blocks = Vec::with_capacity(NL);
        for i in 0..NL {
            let p = format!("transformer.h.{i}");
            blocks.push(Block {
                ln1: ln(&p, "ln_1")?,
                ln2: ln(&p, "ln_2")?,
                attn: lin(&p, "attn.c_attn")?,
                proj: lin(&p, "attn.c_proj")?,
                fc: lin(&p, "mlp.c_fc")?,
                out: lin(&p, "mlp.c_proj")?,
            });
        }
        let wte = QMat::from_gguf(&mut file.borrow_mut(), "transformer.wte.weight")?;
        let wpe = get_v("transformer.wpe.weight")?;
        let work = Work { scratch: vec![0f32; SCRATCH_ELEMS], cache: Cache::new() };
        let model = Self { wte, wpe, blocks, lnf: ln("transformer", "ln_f")?, work: Mutex::new(work) };
        model.check_shapes()?;
        Ok(model)
    }

    /// 全テンソルの形が GPT-2 xsmall と一致するか。ここで通れば、以降の添字・長さの前提 (zip の打ち切りや
    /// スライスの範囲外) が崩れない。外れていれば読み込みの失敗として扱い、engine の順に戻す。
    fn check_shapes(&self) -> R<()> {
        let mat = |m: &QMat, rows: usize, cols: usize| -> R<()> {
            let ok = m.rows == rows
                && m.cols == cols
                && m.scales.len() == rows * cols / 32
                && m.qs.len() == rows * cols;
            if ok { Ok(()) } else { Err("unexpected weight shape") }
        };
        let lin = |l: &Linear, rows: usize, cols: usize| -> R<()> {
            mat(&l.w, rows, cols)?;
            if l.b.len() == rows { Ok(()) } else { Err("unexpected bias length") }
        };
        let ln = |l: &Ln| -> R<()> {
            if l.w.len() == DM && l.b.len() == DM { Ok(()) } else { Err("unexpected layer norm length") }
        };
        if self.blocks.len() != NL {
            return Err("unexpected layer count");
        }
        for blk in &self.blocks {
            ln(&blk.ln1)?;
            ln(&blk.ln2)?;
            lin(&blk.attn, 3 * DM, DM)?;
            lin(&blk.proj, DM, DM)?;
            lin(&blk.fc, FF, DM)?;
            lin(&blk.out, DM, FF)?;
        }
        ln(&self.lnf)?;
        // 語彙は BOS / EOS を含み、少なくとも 1 塊の出力射影ができること。
        if self.wte.cols != DM || self.wte.rows <= (EOS as usize).max(BOS as usize) {
            return Err("unexpected embedding shape");
        }
        mat(&self.wte, self.wte.rows, DM)?;
        if self.wpe.len() % DM != 0 || self.wpe.len() / DM <= MAX_TOKENS + 1 {
            return Err("unexpected position table");
        }
        Ok(())
    }

    /// 読み込み後ずっと生きる大きな領域 (q8 の重みと尺度、f32 のテンソル、f32 に戻す作業領域) を物理メモリに留める。
    /// IME は入力の合間に App Nap で眠り、その間に OS が重みを圧縮すると次の打鍵で展開待ちになるため。
    /// K / V の木はノードごとに伸び縮みするので対象外。戻り値は (留めようとしたバイト数, 全部成功したか)。
    fn lock_resident(&self) -> (usize, bool) {
        let work = self.lock_work();
        let mut f32s: Vec<&[f32]> = vec![&self.wpe, &self.lnf.w, &self.lnf.b, &work.scratch, &self.wte.scales];
        let mut u8s: Vec<&[u8]> = vec![&self.wte.qs];
        for blk in &self.blocks {
            f32s.extend([&blk.ln1.w[..], &blk.ln1.b, &blk.ln2.w, &blk.ln2.b]);
            for lin in [&blk.attn, &blk.proj, &blk.fc, &blk.out] {
                f32s.extend([&lin.w.scales[..], &lin.b]);
                u8s.push(&lin.w.qs);
            }
        }
        // 作業領域は長さが固定 (作り直さない) なので、ここで留めた番地が使われ続ける。
        let bytes = f32s.iter().map(|v| std::mem::size_of_val(*v)).chain(u8s.iter().map(|v| v.len())).sum();
        let ok = f32s.iter().fold(true, |ok, v| crate::platform::lock_memory(v) && ok);
        let ok = u8s.iter().fold(ok, |ok, v| crate::platform::lock_memory(v) && ok);
        (bytes, ok)
    }

    /// 木に溜めた分を捨てる。
    fn clear_cache(&self) {
        self.lock_work().cache = Cache::new();
    }

    /// 作業領域のロック。毒された (= 以前の呼び出しが途中で panic した) なら、木は信用できないので作り直す。
    fn lock_work(&self) -> std::sync::MutexGuard<'_, Work> {
        self.work.lock().unwrap_or_else(|poisoned| {
            let mut guard = poisoned.into_inner();
            guard.cache = Cache::new();
            guard
        })
    }

    /// 各系列 (BOS に続くトークン列。EOS は含めない) の対数尤度 (BOS の後から EOS まで) の合計と、今回新しく計算したトークン数。
    /// 共通の接頭辞と前回までに計算した分は木から使い回し、足りない分だけを 1 まとめにして流す。
    fn score(&self, seqs: &[Vec<u32>]) -> R<(Vec<f64>, usize)> {
        let mut guard = self.lock_work();
        let work: &mut Work = &mut guard;
        // forward の途中で panic すると、K / V が 0・対数確率が NaN のノードが木に残り、後の呼び出しがそれを信用してしまう。
        // panic も失敗も、木を捨てて Err にする (呼び出し側はその文字では engine の順に戻す)。
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.score_in(seqs, work)))
            .unwrap_or(Err("panic while scoring"));
        if r.is_err() {
            work.cache = Cache::new();
        }
        r
    }

    fn score_in(&self, seqs: &[Vec<u32>], work: &mut Work) -> R<(Vec<f64>, usize)> {
        let Work { scratch, cache } = work;
        if seqs.iter().flatten().any(|&t| t as usize >= self.wte.rows || t == BOS || t == EOS) {
            return Err("token id out of range");
        }
        cache.stamp += 1;
        let mut pending: Vec<u32> = Vec::new();
        if cache.nodes.is_empty() {
            cache.nodes.push(Node {
                parent: 0,
                tok: BOS,
                depth: 0,
                stamp: 0,
                children: Vec::new(),
                kv: Vec::new(),
                lp: 0.0,
                eos_lp: f32::NAN,
            });
            pending.push(0);
        }
        let stamp = cache.stamp;
        cache.nodes[0].stamp = stamp;
        let mut ends = Vec::with_capacity(seqs.len());
        for s in seqs {
            let mut cur = 0u32;
            for &t in s {
                cur = cache.child(cur, t, &mut pending);
            }
            ends.push(cur);
        }
        let new_rows = pending.len();
        if !pending.is_empty() {
            self.forward(cache, &pending, scratch);
        }
        self.fill_logprobs(cache, &pending, &ends, scratch);
        let out = ends
            .iter()
            .map(|&end| {
                let lps: f32 = cache.path(end).iter().map(|&i| cache.nodes[i as usize].lp).sum();
                f64::from(lps) + f64::from(cache.nodes[end as usize].eos_lp)
            })
            .collect();
        cache.evict();
        Ok((out, new_rows))
    }

    /// `pending` のノード (親が先) を 1 まとめにして全層に流し、K / V と隠れ状態を書く。
    fn forward(&self, cache: &mut Cache, pending: &[u32], scratch: &mut [f32]) {
        let m = pending.len();
        let paths: Vec<Vec<u32>> = pending.iter().map(|&p| cache.path(p)).collect();
        let mut x = vec![0f32; m * DM];
        #[cfg(test)]
        if tests::INJECT_PANIC.with(Cell::get) {
            cache.nodes[pending[0] as usize].kv = vec![0f32; KV_LEN];
            panic!("injected failure in forward");
        }
        for (row, &p) in x.chunks_exact_mut(DM).zip(pending) {
            let (tok, depth) = (cache.nodes[p as usize].tok as usize, cache.nodes[p as usize].depth as usize);
            self.wte.dequant_rows(tok, tok + 1, row);
            for (v, &pe) in row.iter_mut().zip(&self.wpe[depth * DM..(depth + 1) * DM]) {
                *v += pe;
            }
            cache.nodes[p as usize].kv = vec![0f32; KV_LEN];
        }
        for (l, blk) in self.blocks.iter().enumerate() {
            let qkv = blk.attn.fwd(&blk.ln1.fwd(&x), m, scratch);
            for (&p, row) in pending.iter().zip(qkv.chunks_exact(3 * DM)) {
                let kv = &mut cache.nodes[p as usize].kv;
                kv[l * 2 * DM..(l * 2 + 2) * DM].copy_from_slice(&row[DM..]);
            }
            let nodes = &cache.nodes;
            let mut att = vec![0f32; m * DM];
            let one = |i: usize, out: &mut [f32]| attend(nodes, &paths[i], &qkv[i * 3 * DM..i * 3 * DM + DM], l, out);
            if m >= PAR_ATTN_ROWS {
                att.par_chunks_mut(DM).enumerate().for_each(|(i, out)| one(i, out));
            } else {
                att.chunks_exact_mut(DM).enumerate().for_each(|(i, out)| one(i, out));
            }
            for (v, a) in x.iter_mut().zip(blk.proj.fwd(&att, m, scratch)) {
                *v += a;
            }
            let mut h = blk.fc.fwd(&blk.ln2.fwd(&x), m, scratch);
            h.iter_mut().for_each(|v| *v = gelu_new(*v));
            for (v, a) in x.iter_mut().zip(blk.out.fwd(&h, m, scratch)) {
                *v += a;
            }
        }
        let hid = self.lnf.fwd(&x);
        for (&p, row) in pending.iter().zip(hid.chunks_exact(DM)) {
            cache.nodes[p as usize].kv[NL * 2 * DM..].copy_from_slice(row);
        }
    }

    /// 新しいノードの `lp` と、系列の末尾ノードの `eos_lp` (未計算のもの) を出力射影で求める。
    /// 同じ親の隠れ状態は 1 行にまとめる。
    fn fill_logprobs(&self, cache: &mut Cache, pending: &[u32], ends: &[u32], scratch: &mut [f32]) {
        let mut rows: Vec<u32> = Vec::new();
        let row_of = |rows: &mut Vec<u32>, src: u32| -> usize {
            rows.iter().position(|&r| r == src).unwrap_or_else(|| {
                rows.push(src);
                rows.len() - 1
            })
        };
        // (行, 正解トークン, 書き込み先ノード, EOS か)
        let mut wants: Vec<(usize, u32, u32, bool)> = Vec::new();
        for &p in pending.iter().filter(|&&p| p != 0) {
            let n = &cache.nodes[p as usize];
            let (parent, tok) = (n.parent, n.tok);
            wants.push((row_of(&mut rows, parent), tok, p, false));
        }
        let mut seen: Vec<u32> = Vec::new();
        for &end in ends {
            if cache.nodes[end as usize].eos_lp.is_nan() && !seen.contains(&end) {
                seen.push(end);
                wants.push((row_of(&mut rows, end), EOS, end, true));
            }
        }
        if wants.is_empty() {
            return;
        }
        let mut xs = Vec::with_capacity(rows.len() * DM);
        for &r in &rows {
            xs.extend_from_slice(&cache.nodes[r as usize].kv[NL * 2 * DM..]);
        }
        let targets: Vec<(usize, u32)> = wants.iter().map(|w| (w.0, w.1)).collect();
        let lps = self.target_logprobs(&xs, rows.len(), &targets, scratch);
        for (w, lp) in wants.iter().zip(lps) {
            let n = &mut cache.nodes[w.2 as usize];
            if w.3 {
                n.eos_lp = lp;
            } else {
                n.lp = lp;
            }
        }
    }

    /// `xs` ([r, DM] の隠れ状態) の各行での語彙全体の softmax のもと、`wants` の (行, トークン) の対数確率。
    /// 行数が少なければ q8_0 のまま整数の内積、多ければ語彙を塊に分けて f32 に戻して sgemm。
    fn target_logprobs(&self, xs: &[f32], r: usize, wants: &[(usize, u32)], scratch: &mut [f32]) -> Vec<f32> {
        // 行ごとの (最大値, exp の和)、と要求ごとの正解の logit
        let (state, tl) = if r <= SMALL_R {
            self.logits_small(xs, r, wants)
        } else {
            self.logits_sgemm(xs, r, wants, scratch)
        };
        wants.iter().zip(tl).map(|(w, t)| t - (state[w.0].0 + state[w.0].1.ln())).collect()
    }

    fn logits_small(&self, xs: &[f32], r: usize, wants: &[(usize, u32)]) -> (Vec<(f32, f32)>, Vec<f32>) {
        let xq = quantize_rows(xs, r, DM);
        let nchunk = self.wte.rows.div_ceil(VOCAB_SMALL_CHUNK);
        let parts: Vec<(Vec<(f32, f32)>, Vec<(usize, f32)>)> = (0..nchunk)
            .into_par_iter()
            .map(|ci| {
                let (v0, v1) = (ci * VOCAB_SMALL_CHUNK, ((ci + 1) * VOCAB_SMALL_CHUNK).min(self.wte.rows));
                let mut yt = vec![0f32; (v1 - v0) * 2 * r];
                self.wte.dot_rows(v0, v1, &xq, 2 * r, &mut yt);
                // 上位と残りを足して [語彙, r] の logits にする
                let yt: Vec<f32> = yt.chunks_exact(2 * r).flat_map(|c| (0..r).map(|i| c[i] + c[r + i])).collect();
                let st: Vec<(f32, f32)> = (0..r)
                    .map(|i| {
                        let col = yt.iter().skip(i).step_by(r);
                        let mx = col.clone().copied().fold(f32::NEG_INFINITY, f32::max);
                        (mx, col.map(|&v| (v - mx).exp()).sum())
                    })
                    .collect();
                let hits = wants
                    .iter()
                    .enumerate()
                    .filter(|(_, w)| (v0..v1).contains(&(w.1 as usize)))
                    .map(|(wi, w)| (wi, yt[(w.1 as usize - v0) * r + w.0]))
                    .collect();
                (st, hits)
            })
            .collect();
        let mut state = vec![(f32::NEG_INFINITY, 0f32); r];
        let mut tl = vec![0f32; wants.len()];
        for (st, hits) in parts {
            for (acc, (mx, sum)) in state.iter_mut().zip(st) {
                let m = acc.0.max(mx);
                acc.1 = acc.1 * (acc.0 - m).exp() + sum * (mx - m).exp();
                acc.0 = m;
            }
            for (wi, v) in hits {
                tl[wi] = v;
            }
        }
        (state, tl)
    }

    /// 語彙を塊に分け、塊ごとに f32 へ戻して sgemm し、log-sum-exp を流しながら更新する
    /// ([行 x 語彙] の logits は作らない)。
    fn logits_sgemm(&self, xs: &[f32], n: usize, wants: &[(usize, u32)], scratch: &mut [f32]) -> (Vec<(f32, f32)>, Vec<f32>) {
        let mut logits = vec![0f32; n * VOCAB_CHUNK];
        let mut state = vec![(f32::NEG_INFINITY, 0f32); n];
        let mut tl = vec![0f32; wants.len()];
        self.wte.stream_rows(VOCAB_CHUNK, scratch, |v0, v1, w| {
            let cs = v1 - v0;
            crate::platform::sgemm_abt(n, cs, DM, xs, DM, w, DM, &mut logits, cs);
            state.par_iter_mut().zip(logits.par_chunks(cs)).for_each(|(st, row)| {
                let row = &row[..cs];
                let cmax = row.iter().copied().fold(f32::NEG_INFINITY, f32::max);
                let m = st.0.max(cmax);
                let sum: f32 = row.iter().map(|&v| (v - m).exp()).sum();
                st.1 = st.1 * (st.0 - m).exp() + sum;
                st.0 = m;
            });
            for (t, w) in tl.iter_mut().zip(wants) {
                if (v0..v1).contains(&(w.1 as usize)) {
                    *t = logits[w.0 * cs + w.1 as usize - v0];
                }
            }
        });
        (state, tl)
    }
}

/// 位置 `path` の末尾 (= 今回のトークン) の query `q` の、層 `l` での注意の出力を `out` に書く。
fn attend(nodes: &[Node], path: &[u32], q: &[f32], l: usize, out: &mut [f32]) {
    let hd = DM / NH;
    let scale = 1.0 / (hd as f32).sqrt();
    let mut sc = vec![0f32; path.len()];
    for h in 0..NH {
        let qh = &q[h * hd..(h + 1) * hd];
        for (s, &nid) in sc.iter_mut().zip(path) {
            let k = &nodes[nid as usize].kv[l * 2 * DM + h * hd..][..hd];
            *s = qh.iter().zip(k).map(|(a, b)| a * b).sum::<f32>() * scale;
        }
        let mx = sc.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let mut total = 0f32;
        for s in sc.iter_mut() {
            *s = (*s - mx).exp();
            total += *s;
        }
        let oh = &mut out[h * hd..(h + 1) * hd];
        oh.fill(0.0);
        for (&p, &nid) in sc.iter().zip(path) {
            let v = &nodes[nid as usize].kv[(l * 2 + 1) * DM + h * hd..][..hd];
            for (o, &x) in oh.iter_mut().zip(v) {
                *o += p * x;
            }
        }
        let inv = 1.0 / total;
        oh.iter_mut().for_each(|o| *o *= inv);
    }
}

struct Judge {
    model: Model,
    tok: Tokenizer,
}

impl Judge {
    fn open(dir: &Path) -> R<Self> {
        // 行列積のスレッド数を絞る (既に別の用途で作られていれば無視される)。
        // worker は起動元の QoS を引き継ぐので、明示的に interactive にする。
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(THREADS)
            .start_handler(|_| crate::platform::set_qos_user_interactive())
            .build_global();
        let model = Model::load(&dir.join("model_q8_0.gguf"))?;
        let tok = Tokenizer::from_file(dir.join("tokenizer.json")).map_err(fail("cannot load tokenizer"))?;
        let judge = Self { model, tok };
        // 遅延初期化 (Accelerate・スレッドプール) を読み込みの時間に含める。
        judge.model.score(&[vec![100], vec![100, 101]])?;
        judge.model.clear_cache();
        Ok(judge)
    }

    fn sequence(&self, text: &str) -> R<Vec<u32>> {
        let enc = self.tok.encode(text.to_lowercase(), false).map_err(fail("cannot tokenize"))?;
        Ok(enc.get_ids().to_vec())
    }

    /// 各文の対数尤度 (BOS から EOS まで) と、今回新しく計算したトークン数。
    fn score(&self, texts: &[&str]) -> R<(Vec<f64>, usize)> {
        let seqs = texts.iter().map(|t| self.sequence(t)).collect::<R<Vec<_>>>()?;
        if seqs.iter().any(|s| s.len() + 2 > MAX_TOKENS) {
            return Err("too long");
        }
        self.model.score(&seqs)
    }
}

static JUDGE: OnceLock<Option<Judge>> = OnceLock::new();
/// 読み込みの失敗と採点の失敗は別々に 1 度ずつログする (片方が出ても、もう片方を隠さない)。
static LOAD_LOGGED: AtomicBool = AtomicBool::new(false);
static SCORE_LOGGED: AtomicBool = AtomicBool::new(false);
static LOCK_LOGGED: AtomicBool = AtomicBool::new(false);
/// 読み込み後に物理メモリへ留めた (バイト数, 全部成功したか)。
static LOCKED: OnceLock<(usize, bool)> = OnceLock::new();
/// `preload` で背景読み込みを始めた。立っている間、読み込み中の `rerank` は待たずに審判なしで返す。
static PRELOADED: AtomicBool = AtomicBool::new(false);

thread_local! {
    static LAST_MS: Cell<Option<f64>> = const { Cell::new(None) };
}

/// `flag` ごとに 1 度だけログする。`message` は固定文字列 (入力文字列や外部ライブラリのエラー文を入れない)。
fn log_once(flag: &AtomicBool, message: &str) {
    if !flag.swap(true, Ordering::Relaxed) {
        crate::platform::log(message);
    }
}

fn judge() -> Option<&'static Judge> {
    JUDGE
        .get_or_init(|| {
            let Some(dir) = crate::config::get().judge_dir.clone() else {
                log_once(&LOAD_LOGGED, "judge: not configured; conversion keeps the engine order");
                return None;
            };
            let t0 = Instant::now();
            // 読み込みの panic は、結果を `None` として確定させる (そうしないと preload の背景スレッドが落ちて
            // 初期化されないまま、`rerank` が黙って `None` を返し続ける)。
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| Judge::open(&dir))) {
                Ok(Ok(j)) => {
                    crate::trace::record("judge-load", &format!("ms={:.0}", t0.elapsed().as_secs_f64() * 1e3));
                    let (bytes, ok) = j.model.lock_resident();
                    if !ok {
                        log_once(&LOCK_LOGGED, "judge: could not lock model memory; the judge runs without the lock");
                    }
                    let _ = LOCKED.set((bytes, ok));
                    crate::trace::record("judge-locked", &format!("mb={} ok={}", bytes >> 20, u8::from(ok)));
                    Some(j)
                }
                Ok(Err(err)) => {
                    log_once(&LOAD_LOGGED, &format!("judge: model unavailable; conversion keeps the engine order ({err})"));
                    None
                }
                Err(_) => {
                    log_once(&LOAD_LOGGED, "judge: model load panicked; conversion keeps the engine order");
                    None
                }
            }
        })
        .as_ref()
}

/// モデルを背景スレッドで読み始める (起動時に 1 度)。呼んだスレッドは待たない。
/// 読み終わる前の `rerank` は審判なし (engine の順) で返す。
pub fn preload() {
    if PRELOADED.swap(true, Ordering::Relaxed) {
        return;
    }
    if std::thread::Builder::new()
        .name("judge-preload".to_string())
        .spawn(|| {
            crate::platform::set_qos_user_interactive();
            let _ = judge();
        })
        .is_err()
    {
        PRELOADED.store(false, Ordering::Relaxed);
        log_once(&LOAD_LOGGED, "judge: preload thread failed to start");
    }
}

/// 読み込み後に物理メモリへ留めた (バイト数, 全部成功したか)。読み込み前や審判なしなら `None`。
pub fn locked() -> Option<(usize, bool)> {
    LOCKED.get().copied()
}

/// 直近の `rerank` にかかった時間 (ms)。取り出すと消える。
pub fn take_last_ms() -> Option<f64> {
    LAST_MS.with(Cell::take)
}

/// engine のスコアと審判の対数尤度の合計が大きい順の添字。どれかが有限でなければ `None`
/// (NaN が混じると並べ替えが全順序にならず、順番が不定になるため)。
fn order_by_total(engine_scores: &[f32], judge_scores: &[f64]) -> Option<Vec<usize>> {
    let combined: Vec<f64> = engine_scores.iter().zip(judge_scores).map(|(&b, &j)| f64::from(b) + WEIGHT * j).collect();
    if combined.len() != engine_scores.len() || combined.iter().any(|v| !v.is_finite()) {
        return None;
    }
    let mut order: Vec<usize> = (0..combined.len()).collect();
    order.sort_by(|&a, &b| combined[b].total_cmp(&combined[a]));
    Some(order)
}

/// 全文候補 `texts` を `engine_scores` (大きいほど良い) + 審判の対数尤度で並べ替えた添字 (良い順)。
/// 審判が使えない・失敗したときは `None` (呼び出し側は engine の順のままにする)。
pub fn rerank(texts: &[&str], engine_scores: &[f32]) -> Option<Vec<usize>> {
    if texts.len() < 2 || texts.len() != engine_scores.len() {
        return None;
    }
    let judge = if PRELOADED.load(Ordering::Relaxed) {
        // 背景で読み込み中なら待たない。
        match JUDGE.get() {
            None => {
                crate::trace::record("judge-skip", "loading=1");
                return None;
            }
            Some(None) => {
                // 読み込みは終わったが使えない。理由は読み込み側がログ済み (まだなら固定文を 1 度)。
                log_once(&LOAD_LOGGED, "judge: unavailable; conversion keeps the engine order");
                return None;
            }
            Some(Some(j)) => j,
        }
    } else {
        judge()?
    };
    let t0 = Instant::now();
    let (scores, rows) = match judge.score(texts) {
        Ok(s) => s,
        Err(err) => {
            log_once(&SCORE_LOGGED, &format!("judge: scoring failed; this key keeps the engine order ({err})"));
            return None;
        }
    };
    let ms = t0.elapsed().as_secs_f64() * 1e3;
    LAST_MS.with(|c| c.set(Some(ms)));
    crate::trace::record("judge", &format!("ms={ms:.1} n={} rows={rows}", texts.len()));
    order_by_total(engine_scores, &scores)
}

/// `--judge-bench <testset.tsv>`: 読み (1 列目) と正解 (2 列目、`|` 区切り) の表を、
/// 実際のエンジン + 審判の経路で変換して top-1 と時間を出す。
pub fn bench_cli(path: &str) -> i32 {
    use crate::composer::CandidateEngine;
    let Ok(text) = std::fs::read_to_string(path) else {
        eprintln!("cannot read {path}");
        return 2;
    };
    let Some(core) = crate::kkc::shared_core() else {
        eprintln!("dictionary unavailable");
        return 2;
    };
    let mut engine = crate::kkc::RustEngine::new();
    let t0 = Instant::now();
    if judge().is_none() {
        eprintln!("judge unavailable (set NOVAKEY_JUDGE_DIR)");
        return 2;
    }
    println!("load {:.0} ms", t0.elapsed().as_secs_f64() * 1e3);
    let (mut n, mut base_ok, mut judged_ok) = (0usize, 0usize, 0usize);
    let mut ms = Vec::new();
    let mut rev = 0;
    for line in text.lines() {
        let Some((reading, gold)) = line.split_once('\t') else { continue };
        let gold: Vec<&str> = gold.split('|').collect();
        rev += 1;
        // 審判なしの engine の順 (比較の基準) と、実際の経路 (審判あり)。
        let base: Vec<String> = core.convert_all(reading).main.iter().map(|c| c.text.replace('\u{1F}', "")).collect();
        let live = engine.convert(rev, reading);
        if let Some(m) = take_last_ms() {
            ms.push(m);
        }
        n += 1;
        base_ok += usize::from(base.first().is_some_and(|c| gold.contains(&c.as_str())));
        judged_ok += usize::from(live.first().is_some_and(|c| gold.contains(&c.as_str())));
    }
    ms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let pct = |p: f64| ms.get(((ms.len() as f64 - 1.0) * p).round() as usize).copied().unwrap_or(0.0);
    println!("n={n} engine top1={base_ok} judged top1={judged_ok}");
    println!("judge ms over {} decisions: median {:.1} p95 {:.1} max {:.1}", ms.len(), pct(0.5), pct(0.95), pct(1.0));
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        /// 立っている間、`forward` が K / V を 0 で確保した直後に panic する (途中失敗の再現)。
        pub(super) static INJECT_PANIC: Cell<bool> = const { Cell::new(false) };
    }

    fn rand_qmat(rows: usize, cols: usize, st: &mut u64) -> QMat {
        let nb = rows * cols / 32;
        let qs: Vec<u8> = (0..nb * 32).map(|_| (lcg(st) * 127.0) as i8 as u8).collect();
        let scales: Vec<f32> = (0..nb).map(|_| 0.0005 + lcg(st).abs() * 0.002).collect();
        QMat { rows, cols, qs, scales }
    }

    fn rand_vec(n: usize, st: &mut u64) -> Vec<f32> {
        (0..n).map(|_| lcg(st) * 0.1).collect()
    }

    fn rand_ln(st: &mut u64) -> Ln {
        Ln { w: vec![1.0; DM].iter().map(|v| v + lcg(st) * 0.05).collect(), b: rand_vec(DM, st) }
    }

    fn rand_lin(rows: usize, cols: usize, st: &mut u64) -> Linear {
        Linear { w: rand_qmat(rows, cols, st), b: rand_vec(rows, st) }
    }

    /// 形だけ本物と同じ、乱数の重みの小さなモデル (語彙 64)。
    fn synthetic_model() -> Model {
        let mut st = 99u64;
        let blocks = (0..NL)
            .map(|_| Block {
                ln1: rand_ln(&mut st),
                ln2: rand_ln(&mut st),
                attn: rand_lin(3 * DM, DM, &mut st),
                proj: rand_lin(DM, DM, &mut st),
                fc: rand_lin(FF, DM, &mut st),
                out: rand_lin(DM, FF, &mut st),
            })
            .collect();
        let wte = rand_qmat(64, DM, &mut st);
        let wpe = rand_vec((MAX_TOKENS + 2) * DM, &mut st);
        let work = Work { scratch: vec![0f32; SCRATCH_ELEMS], cache: Cache::new() };
        Model { wte, wpe, blocks, lnf: rand_ln(&mut st), work: Mutex::new(work) }
    }


    #[test]
    fn 形の揃ったモデルは検査を通り_どこかの形が外れると失敗する() {
        let mut m = synthetic_model();
        assert_eq!(m.check_shapes(), Ok(()));
        m.blocks[3].fc.b.pop();
        assert!(m.check_shapes().is_err(), "bias");
        let mut m = synthetic_model();
        m.blocks[0].ln2.w.push(0.0);
        assert!(m.check_shapes().is_err(), "layer norm");
        let mut m = synthetic_model();
        m.blocks[5].out.w = rand_qmat(DM, FF + 32, &mut 1);
        assert!(m.check_shapes().is_err(), "weight");
        let mut m = synthetic_model();
        m.lnf.b.pop();
        assert!(m.check_shapes().is_err(), "final layer norm");
        let mut m = synthetic_model();
        m.wpe.truncate(DM * 10);
        assert!(m.check_shapes().is_err(), "position table");
    }

    /// forward の途中で panic しても、木は捨てられて Err になり、次の呼び出しは壊れた結果を信用せず正しい値を返す。
    #[test]
    fn 採点の途中で_panic_しても木を捨て_次の呼び出しは正しい() {
        let m = synthetic_model();
        let seqs = vec![vec![10, 11, 12], vec![10, 11, 13]];
        let want = m.score(&seqs).unwrap().0;
        m.clear_cache();
        INJECT_PANIC.with(|c| c.set(true));
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let failed = m.score(&seqs);
        std::panic::set_hook(prev_hook);
        INJECT_PANIC.with(|c| c.set(false));
        assert!(failed.is_err());
        assert_eq!(m.lock_work().cache.nodes.len(), 0, "壊れた木は捨てる");
        let got = m.score(&seqs).unwrap().0;
        for (g, w) in got.iter().zip(&want) {
            assert!((g - w).abs() < 1e-3, "{got:?} {want:?}");
        }
        assert!(got.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn 毒されたロックでも_木を作り直して使える() {
        let m = synthetic_model();
        let seqs = vec![vec![10, 11]];
        let want = m.score(&seqs).unwrap().0;
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut g = m.work.lock().unwrap();
            g.cache.nodes[0].kv = vec![0.0; KV_LEN];
            g.cache.nodes[1].lp = f32::NAN;
            std::panic::resume_unwind(Box::new("poison"));
        }));
        assert!(m.work.is_poisoned());
        let got = m.score(&seqs).unwrap().0;
        assert!((got[0] - want[0]).abs() < 1e-3);
    }

    #[test]
    fn 合計が有限でないときは並べ替えず_有限なら大きい順() {
        assert_eq!(order_by_total(&[0.0, 0.0, 0.0], &[-3.0, -1.0, -2.0]), Some(vec![1, 2, 0]));
        assert_eq!(order_by_total(&[-1.0, 0.0], &[0.0, -0.5]), Some(vec![1, 0]));
        assert_eq!(order_by_total(&[0.0, 0.0], &[f64::NAN, -1.0]), None);
        assert_eq!(order_by_total(&[f32::INFINITY, 0.0], &[0.0, -1.0]), None);
        assert_eq!(order_by_total(&[0.0, f32::NAN], &[0.0, -1.0]), None);
    }

    fn lcg(state: &mut u64) -> f32 {
        *state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        ((*state >> 33) as f32 / (1u64 << 31) as f32) * 2.0 - 1.0
    }

    /// 整数の内積の経路が、f32 に戻した重みでの普通の行列積に (外れ値入りの入力でも) 近い。
    #[test]
    fn matmul_small_matches_f32_reference() {
        let (rows, cols, m) = (37, 128, 5);
        let mut st = 7u64;
        let nb = rows * cols / 32;
        let qs: Vec<u8> = (0..nb * 32).map(|_| (lcg(&mut st) * 127.0) as i8 as u8).collect();
        let scales: Vec<f32> = (0..nb).map(|_| 0.001 + lcg(&mut st).abs() * 0.02).collect();
        let w = QMat { rows, cols, qs, scales };
        let mut x: Vec<f32> = (0..m * cols).map(|_| lcg(&mut st) * 3.0).collect();
        x[5] = 120.0; // 外れ値
        x[cols + 70] = -90.0;
        let mut wf = vec![0f32; rows * cols];
        w.dequant_rows(0, rows, &mut wf);
        let mut got = vec![0f32; m * rows];
        w.matmul_small(&x, m, &mut got);
        for i in 0..m {
            for r in 0..rows {
                let want: f32 = (0..cols).map(|c| x[i * cols + c] * wf[r * cols + c]).sum();
                let g = got[i * rows + r];
                assert!((g - want).abs() <= 2e-3 * (1.0 + want.abs()), "[{i},{r}] got {g} want {want}");
            }
        }
    }

    fn leaf(parent: u32, tok: u32, depth: u32, stamp: u64) -> Node {
        Node { parent, tok, depth, stamp, children: Vec::new(), kv: vec![0.0], lp: 0.0, eos_lp: f32::NAN }
    }

    /// 共通の接頭辞は同じノードを通り、上限を超えたら古い葉から捨てる (根と今回のノードは残る)。
    #[test]
    fn cache_shares_prefixes_and_evicts_old_leaves() {
        let mut c = Cache::new();
        c.nodes.push(leaf(0, BOS, 0, 0));
        c.stamp = 1;
        let mut pending = Vec::new();
        let a = c.child(0, 10, &mut pending);
        let b = c.child(a, 11, &mut pending);
        let a2 = c.child(0, 10, &mut pending);
        let b2 = c.child(a2, 11, &mut pending);
        assert_eq!(b, b2);
        assert_eq!(pending.len(), 2);
        assert_eq!(c.path(b), vec![0, a, b]);
        // 古い世代の葉をたくさん作ってから、新しい世代で 1 本だけ使う
        for t in 0..(MAX_NODES as u32 + 50) {
            let id = c.child(a, 1000 + t, &mut pending);
            c.nodes[id as usize].kv = vec![0.0];
        }
        c.stamp = 2;
        let keep = c.child(0, 10, &mut pending);
        let keep2 = c.child(keep, 11, &mut pending);
        c.nodes[a as usize].kv = vec![0.0];
        c.nodes[b as usize].kv = vec![0.0];
        c.evict();
        assert!(c.live() <= MAX_NODES);
        assert_eq!(c.nodes[0].tok, BOS);
        assert_eq!(c.path(keep2), vec![0, a, b]);
    }
}
