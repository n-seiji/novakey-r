//! LOUDS (Level-Order Unary Degree Sequence) の読み込みと子ノード探索。
#![forbid(unsafe_code)]

use std::ops::Range;

/// ビット列は u64 の配列で、各ワード内は MSB 先頭。末尾は 1 で埋められている。
/// ノード 0 は super-root、ノード 1 が root。ノード p の子は「p 番目の 0 の直後から次の 0 まで」の 1 の並びで、
/// 先頭の子のノード番号は `(その位置より左の 1 の数) + 1`。
#[derive(Debug)]
pub struct Louds {
    bits: Vec<u64>,
    /// `rank0[i]` = ワード i より左にある 0 の数 (長さ bits.len() + 1)
    rank0: Vec<u32>,
    /// ノード番号 -> 文字 ID (`.loudschars2`)
    node_chars: Vec<u8>,
}

impl Louds {
    /// `.louds` (u64 LE 配列) と `.loudschars2` (ノードごとの u8) から作る。
    pub fn from_bytes(louds: &[u8], node_chars: Vec<u8>) -> Result<Self, String> {
        if louds.is_empty() || louds.len() % 8 != 0 {
            return Err(format!("louds のサイズが 8 の倍数ではありません ({} bytes)", louds.len()));
        }
        let bits: Vec<u64> = louds
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]))
            .collect();
        Ok(Self::from_words(bits, node_chars))
    }

    pub fn from_words(bits: Vec<u64>, node_chars: Vec<u8>) -> Self {
        let mut rank0 = Vec::with_capacity(bits.len() + 1);
        let mut acc: u32 = 0;
        rank0.push(0);
        for w in &bits {
            acc += w.count_zeros();
            rank0.push(acc);
        }
        Self { bits, rank0, node_chars }
    }

    fn total_bits(&self) -> usize {
        self.bits.len() * 64
    }

    /// 先頭から数えて p (1 始まり) 番目の 0 のビット位置。
    fn select0(&self, p: usize) -> Option<usize> {
        if p == 0 || p > *self.rank0.last()? as usize {
            return None;
        }
        // rank0[i + 1] >= p となる最小の i
        let i = self.rank0[1..].partition_point(|&r| (r as usize) < p);
        let mut remaining = p - self.rank0[i] as usize; // ワード内で見つけたい 0 の個数 (>= 1)
        let mut inv = !self.bits[i];
        while remaining > 1 {
            inv ^= 1u64 << (63 - inv.leading_zeros());
            remaining -= 1;
        }
        Some(i * 64 + inv.leading_zeros() as usize)
    }

    /// 位置 start から連続する 1 の個数。
    fn run_of_ones(&self, start: usize) -> usize {
        let total = self.total_bits();
        let mut pos = start;
        while pos < total {
            let (wi, off) = (pos / 64, pos % 64);
            // 左にずらして空いた下位ビットは 0 → 反転で 1 になるので leading_zeros が off 分を超えたら次のワードへ
            let lz = (!(self.bits[wi] << off)).leading_zeros() as usize;
            let avail = 64 - off;
            if lz < avail {
                return pos + lz - start;
            }
            pos = (wi + 1) * 64;
        }
        total.saturating_sub(start)
    }

    /// ノード `parent` の子ノード番号の範囲。
    pub fn child_range(&self, parent: usize) -> Range<usize> {
        let Some(zero_pos) = self.select0(parent) else {
            return 0..0;
        };
        let after = zero_pos + 1;
        let first_child = after - parent + 1;
        first_child..first_child + self.run_of_ones(after)
    }

    /// `parent` の子のうち文字 ID が `ch` のもの。
    pub fn find_child(&self, parent: usize, ch: u8) -> Option<usize> {
        self.child_range(parent)
            .find(|&n| self.node_chars.get(n) == Some(&ch))
    }

    /// root から文字 ID 列をたどって到達したノード (完全一致検索)。空列は root。
    pub fn search(&self, chars: &[u8]) -> Option<usize> {
        let mut node = 1;
        for &c in chars {
            node = self.find_child(node, c)?;
        }
        Some(node)
    }

    /// root から文字 ID 列を先頭から順にたどり、途中で到達した各ノードを (深さ 0 始まり, ノード) で返す。
    /// 見つからなくなったところで打ち切る。`movingTowardPrefixSearch` の全 prefix 探索に当たる。
    pub fn walk(&self, chars: &[u8]) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut node = 1;
        for (depth, &c) in chars.iter().enumerate() {
            match self.find_child(node, c) {
                Some(n) => {
                    out.push((depth, n));
                    node = n;
                }
                None => break,
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手組みのトライ:  root -> a(2) -> c(4)
    ///                       -> b(3) -> d(5), e(6)
    /// BFS のビット列: "10" (super-root) + root の子 "110" + a の子 "10" + b の子 "110" + c "0" + d "0" + e "0"
    fn sample() -> Louds {
        let s = "10" .to_string() + "110" + "10" + "110" + "0" + "0" + "0";
        let mut bits = 0u64;
        for (i, ch) in s.chars().enumerate() {
            if ch == '1' {
                bits |= 1u64 << (63 - i);
            }
        }
        // 残りは 1 で埋める
        for i in s.len()..64 {
            bits |= 1u64 << (63 - i);
        }
        // node 0 = super-root(0), 1 = root(0), 2 = a(1), 3 = b(2), 4 = c(1), 5 = d(4), 6 = e(5)
        Louds::from_words(vec![bits], vec![0, 0, 1, 2, 1, 4, 5])
    }

    #[test]
    fn child_ranges_follow_bfs_layout() {
        let l = sample();
        assert_eq!(l.child_range(1), 2..4);
        assert_eq!(l.child_range(2), 4..5);
        assert_eq!(l.child_range(3), 5..7);
        assert_eq!(l.child_range(4), 7..7);
        assert_eq!(l.child_range(6), 7..7);
        assert_eq!(l.child_range(0), 0..0);
        assert_eq!(l.child_range(99), 0..0);
    }

    #[test]
    fn search_walks_by_char_id() {
        let l = sample();
        assert_eq!(l.search(&[]), Some(1));
        assert_eq!(l.search(&[1]), Some(2));
        assert_eq!(l.search(&[2, 5]), Some(6));
        assert_eq!(l.search(&[2, 4]), Some(5));
        assert_eq!(l.search(&[1, 2]), None);
        assert_eq!(l.walk(&[2, 4, 9]), vec![(0, 3), (1, 5)]);
    }

    #[test]
    fn select0_and_runs_cross_word_boundaries() {
        // 64 ビットを超える列: 1 を 70 個 -> 0 -> 1 を 3 個 -> 0 ...
        let mut s = String::new();
        s.push_str(&"1".repeat(70));
        s.push('0');
        s.push_str("111");
        s.push('0');
        s.push('0');
        while s.len() % 64 != 0 {
            s.push('1');
        }
        let words: Vec<u64> = s
            .as_bytes()
            .chunks(64)
            .map(|c| c.iter().fold(0u64, |a, &b| (a << 1) | (b == b'1') as u64))
            .collect();
        let l = Louds::from_words(words, vec![0; 80]);
        assert_eq!(l.select0(1), Some(70));
        assert_eq!(l.select0(2), Some(74));
        assert_eq!(l.select0(3), Some(75));
        assert_eq!(l.select0(4), None);
        assert_eq!(l.run_of_ones(0), 70);
        assert_eq!(l.run_of_ones(71), 3);
        // 親 1 の子 = 71 - 1 + 1 = 71 番から 3 個
        assert_eq!(l.child_range(1), 71..74);
        assert_eq!(l.child_range(2), 74..74);
    }
}
