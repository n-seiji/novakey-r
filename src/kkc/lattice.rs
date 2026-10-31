//! ラティスと N-best Viterbi (`FullInputProcessing.swift` / `LatticeNode.swift` / `RegisteredNode.swift`)。
//!
//! 入力は `.direct` (1 文字 = 1 入力) なので、入力 index と surface index は同じで、ノードは surface の
//! 文字位置ごとに並ぶ。スコアは f32 で、Swift と同じ和の順序で足す (ビット一致のため)。
#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::rc::Rc;

use super::dict::{Dictionary, CC_MISSING};
use super::learning::Learning;
use super::elem::{include_mm_value_calculation, is_clause, Elem, MID_BOS_EOS};
use super::wise::wise_dicdata;

/// 辞書のエントリの最大長さ (`DicdataStore.maxlength`)
pub const MAX_LENGTH: usize = 20;

/// 1 回の辞書引きの結果の 1 件。`ruby_chars` は ruby のスカラー数 (= ノードの長さ)。
#[derive(Debug, Clone)]
pub struct Found {
    pub elem: Rc<Elem>,
    pub ruby_chars: usize,
}

/// トライのノードごとの辞書引きのメモ。変換のたびに全再計算するが、辞書引きは使い回す。
/// 1 ノードの Elem は 1 度だけ作るので、大きさは辞書のうち実際に引いた部分で頭打ちになる。
pub struct LookupCache {
    map: HashMap<(char, usize), Rc<[Found]>>,
    limit: usize,
}

const CACHE_LIMIT: usize = 20_000;

impl LookupCache {
    pub fn new() -> Self {
        Self::with_limit(CACHE_LIMIT)
    }

    fn with_limit(limit: usize) -> Self {
        Self { map: HashMap::new(), limit }
    }

    /// `substring` (カタカナ化済み・最大 20 文字) の先頭から始まる辞書語すべて (短い順) を、ノードごとの塊で。
    fn lookup(&mut self, dict: &Dictionary, substring: &str) -> Vec<Rc<[Found]>> {
        let nodes = match dict.prefix_nodes(substring) {
            Ok(n) => n,
            Err(err) => {
                log_lookup_failure(&err);
                return Vec::new();
            }
        };
        let mut out = Vec::with_capacity(nodes.len());
        for (first, node) in nodes {
            if let Some(v) = self.map.get(&(first, node)) {
                out.push(v.clone());
                continue;
            }
            // 読めなかったノードも空で覚え、clear まで読み直さない
            let entries = match dict.node_entries(first, node) {
                Ok(e) => e,
                Err(err) => {
                    log_lookup_failure(&err);
                    Vec::new()
                }
            };
            let found: Rc<[Found]> = entries
                .into_iter()
                .map(|e| {
                    let ruby_chars = e.ruby.chars().count();
                    Found { elem: Rc::new(Elem::new(e.word, e.ruby, e.lcid as usize, e.rcid as usize, e.mid as usize, e.value)), ruby_chars }
                })
                .collect();
            if self.map.len() >= self.limit {
                self.map.clear();
            }
            self.map.insert((first, node), found.clone());
            out.push(found);
        }
        out
    }
}

/// 読み (ユーザーの入力) はログに出さない。毎回出ないよう 1 度だけ。
fn log_lookup_failure(err: &super::dict::DictError) {
    static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        crate::platform::log(&format!("dictionary lookup failed: {}", err.kind()));
    }
}

impl Default for LookupCache {
    fn default() -> Self {
        Self::new()
    }
}

const NONE: u32 = u32::MAX;

#[derive(Debug)]
pub struct Node {
    pub elem: Rc<Elem>,
    /// 終端の surface index (排他的)
    pub end: usize,
    /// 辞書ではなく `getWiseDicdata` で作ったか
    pub wise: bool,
    /// `elem.lcid` / `elem.removed` の写し (遷移先の走査で Rc をたどらないため)
    lcid: usize,
    removed: bool,
    /// prevs が N 個そろっているとき、その中の最小の total。そろっていなければ -inf。
    /// これ以下の値は入れられない。
    full_min: f32,
    /// このノードに繋がる直前のノード (`RegisteredNode`)。値の降順。
    prevs: Vec<u32>,
}

impl Node {
    fn new(elem: Rc<Elem>, end: usize, wise: bool) -> Self {
        Self { lcid: elem.lcid, removed: elem.removed, elem, end, wise, full_min: f32::NEG_INFINITY, prevs: Vec::new() }
    }
}

/// `RegisteredNode`: `node` を終えた時点の経路。`prev` をたどると始点まで戻れる。
#[derive(Debug, Clone, Copy)]
struct Reg {
    node: u32,
    prev: u32,
    total: f32,
}

/// 文節ごとの情報 (`ClauseDataUnit` の使う部分)
#[derive(Debug, Clone)]
pub struct Clause {
    pub mid: usize,
    pub text: String,
    /// この文節を作る `CandidateData.data` の最後の添字 (含む)。未設定は -1。
    pub data_end_index: isize,
}

/// `CandidateData`
#[derive(Debug, Clone)]
pub struct CandidateData {
    /// (文節, その文節の終わりまでの累積スコア)
    pub clauses: Vec<(Clause, f32)>,
    pub data: Vec<Rc<Elem>>,
}

pub struct Lattice {
    /// 添字 0 は始点 (BOS) の番兵ノード
    nodes: Vec<Node>,
    /// start ごとのノード範囲
    ranges: Vec<std::ops::Range<usize>>,
    regs: Vec<Reg>,
    /// EOS に繋がる経路 (上限なし。追加順)
    eos: Vec<u32>,
    surface_count: usize,
}

impl Lattice {
    /// `memory` の学習エントリも辞書の語と同じようにノードにする (辞書の語のあとに並べる)。`chars` はカタカナ化済みの読み。
    pub fn build(dict: &Dictionary, cache: &mut LookupCache, memory: &Learning, chars: &[char]) -> Self {
        let n = chars.len();
        let mut nodes = vec![Node::new(Rc::new(Elem::bos()), 0, false)];
        let mut ranges = Vec::with_capacity(n);
        let mut learned: Vec<Found> = Vec::new();
        for start in 0..n {
            let from = nodes.len();
            let upper = (start + MAX_LENGTH).min(n);
            let substring: String = chars[start..upper].iter().collect();
            let found = cache.lookup(dict, &substring);
            for f in found.iter().flat_map(|g| g.iter()) {
                // 読みが切り出した範囲に収まらない語は来ないはずだが、念のため
                if f.ruby_chars == 0 || start + f.ruby_chars > n {
                    continue;
                }
                nodes.push(Node::new(f.elem.clone(), start + f.ruby_chars, false));
            }
            learned.clear();
            memory.lookup(&substring, &mut learned);
            for f in &learned {
                if start + f.ruby_chars > n {
                    continue;
                }
                nodes.push(Node::new(f.elem.clone(), start + f.ruby_chars, false));
            }
            // 機械的に作るエントリ
            let mut segment = String::new();
            for i in start..upper {
                segment.push(chars[i]);
                for e in wise_dicdata(&segment, start..i + 1, chars) {
                    nodes.push(Node::new(Rc::new(e), i + 1, true));
                }
            }
            ranges.push(from..nodes.len());
        }
        let mut lattice = Self { nodes, ranges, regs: Vec::new(), eos: Vec::new(), surface_count: n };
        // 始点
        lattice.regs.push(Reg { node: 0, prev: NONE, total: 0.0 });
        for idx in lattice.ranges.first().cloned().unwrap_or(0..0) {
            lattice.nodes[idx].prevs.push(0);
        }
        lattice
    }

    pub fn nodes_at(&self, start: usize) -> &[Node] {
        match self.ranges.get(start) {
            Some(r) => &self.nodes[r.clone()],
            None => &[],
        }
    }

    /// `kana2lattice_all` の本体。
    pub fn run(&mut self, dict: &Dictionary, n_best: usize) {
        for start in 0..self.surface_count {
            let is_head = start == 0;
            for idx in self.ranges[start].clone() {
                if self.nodes[idx].prevs.is_empty() {
                    continue;
                }
                if self.nodes[idx].removed {
                    continue;
                }
                let elem = self.nodes[idx].elem.clone();
                let w_value = elem.value();
                // 生起確率 (prevs の各要素に対応)
                let values: Vec<f32> = self.nodes[idx]
                    .prevs
                    .iter()
                    .map(|&r| {
                        let prev = self.regs[r as usize];
                        let total = prev.total + w_value;
                        if is_head {
                            total + dict.cc(self.nodes[prev.node as usize].elem.rcid, elem.lcid)
                        } else {
                            total
                        }
                    })
                    .collect();
                let end = self.nodes[idx].end;
                if end == self.surface_count {
                    // updateResultNode
                    for (i, &v) in values.iter().enumerate() {
                        let prev = self.nodes[idx].prevs[i];
                        let id = self.push_reg(idx, prev, v);
                        self.eos.push(id);
                    }
                } else {
                    self.update_next_nodes(dict, idx, &elem, &values, end, n_best);
                }
            }
        }
    }

    fn push_reg(&mut self, node: usize, prev: u32, total: f32) -> u32 {
        let id = self.regs.len() as u32;
        self.regs.push(Reg { node: node as u32, prev, total });
        id
    }

    /// N-Best を保ちつつ遷移先ノードを更新する (`updateNextNodes`)。
    ///
    /// `values` は降順 (prevs が降順で、同じ定数を足すだけなので)。ある値が入れられない (遷移先の N 個が
    /// すべてそれ以上) なら、それより小さい値も入れられないので、そこで打ち切る (結果は Swift と同じ)。
    fn update_next_nodes(&mut self, dict: &Dictionary, idx: usize, elem: &Elem, values: &[f32], next_start: usize, n_best: usize) {
        let cc_row = dict.cc_row(elem.rcid);
        for next_idx in self.ranges[next_start].clone() {
            let next = &self.nodes[next_idx];
            if next.removed {
                continue;
            }
            let cc_value = cc_row.and_then(|r| r.get(next.lcid).copied()).unwrap_or(CC_MISSING);
            // 最大の値でも入れられない (遷移先の N 個がすべてそれ以上) なら、このノードは丸ごと飛ばせる
            if values.first().is_none_or(|&v| cc_value + v <= next.full_min) {
                continue;
            }
            for (index, &value) in values.iter().enumerate() {
                let new_value = cc_value + value;
                // 追加すべき index: total >= new_value を満たす最後の要素の次
                let lastindex = {
                    let prevs = &self.nodes[next_idx].prevs;
                    prevs.iter().rposition(|&r| self.regs[r as usize].total >= new_value).map_or(0, |p| p + 1)
                };
                if lastindex == n_best {
                    break;
                }
                let prev = self.nodes[idx].prevs[index];
                let id = self.push_reg(idx, prev, new_value);
                let node = &mut self.nodes[next_idx];
                if node.prevs.len() >= n_best {
                    node.prevs.pop();
                }
                node.prevs.insert(lastindex, id);
                node.full_min = if node.prevs.len() >= n_best {
                    node.prevs.last().map_or(f32::NEG_INFINITY, |&r| self.regs[r as usize].total)
                } else {
                    f32::NEG_INFINITY
                };
            }
        }
    }

    /// EOS に繋がる全経路の `CandidateData` (`LatticeNode.getCandidateData`)。
    pub fn candidate_data(&self) -> Vec<CandidateData> {
        self.eos.iter().map(|&r| self.reg_candidate_data(r)).collect()
    }

    /// `RegisteredNode.getCandidateData`
    fn reg_candidate_data(&self, reg: u32) -> CandidateData {
        // 始点までさかのぼってから前向きに処理する
        let mut chain: Vec<Reg> = Vec::with_capacity(8);
        let mut cursor = reg;
        while cursor != NONE {
            let r = self.regs[cursor as usize];
            chain.push(r);
            cursor = r.prev;
        }
        chain.reverse();

        let head = &self.nodes[chain[0].node as usize].elem;
        let mut clauses: Vec<(Clause, f32)> = vec![(Clause { mid: head.mid, text: String::new(), data_end_index: -1 }, 0.0)];
        let mut data: Vec<Rc<Elem>> = Vec::with_capacity(chain.len());
        let mut last = 0usize;

        for i in 1..chain.len() {
            let node = &self.nodes[chain[i].node as usize].elem;
            if node.word.is_empty() {
                continue;
            }
            let prev_node = &self.nodes[chain[i - 1].node as usize].elem;
            if clauses[last].0.text.is_empty() || !is_clause(prev_node.rcid, node.lcid) {
                // 文節の継続
                let clause = &mut clauses[last].0;
                clause.text.push_str(&node.word);
                if (clause.mid == MID_BOS_EOS && node.mid != MID_BOS_EOS) || include_mm_value_calculation(node) {
                    clause.mid = node.mid;
                }
                data.push(node.clone());
                clause.data_end_index = data.len() as isize - 1;
                clauses[last].1 = chain[i].total;
            } else {
                // 文節の境界
                let mut unit = Clause { mid: MID_BOS_EOS, text: node.word.clone(), data_end_index: -1 };
                if include_mm_value_calculation(node) {
                    unit.mid = node.mid;
                }
                data.push(node.clone());
                unit.data_end_index = data.len() as isize - 1;
                clauses.push((unit, chain[i].total));
                last = clauses.len() - 1;
            }
        }
        CandidateData { clauses, data }
    }
}

#[cfg(test)]
mod tests {
    use super::super::dict::to_katakana;
    use super::*;

    fn elem(word: &str, ruby: &str, lcid: usize, rcid: usize, mid: usize, v: f32) -> Rc<Elem> {
        Rc::new(Elem::new(word.into(), ruby.into(), lcid, rcid, mid, v))
    }

    /// 辞書を使わない小さなラティス。nodes: (start, end, elem)
    fn manual(n: usize, defs: Vec<(usize, usize, Rc<Elem>)>) -> Lattice {
        let mut nodes = vec![Node::new(Rc::new(Elem::bos()), 0, false)];
        let mut ranges = Vec::new();
        for start in 0..n {
            let from = nodes.len();
            for (_, e, el) in defs.iter().filter(|d| d.0 == start) {
                nodes.push(Node::new(el.clone(), *e, false));
            }
            ranges.push(from..nodes.len());
        }
        let mut l = Lattice { nodes, ranges, regs: vec![Reg { node: 0, prev: NONE, total: 0.0 }], eos: Vec::new(), surface_count: n };
        for idx in l.ranges[0].clone() {
            l.nodes[idx].prevs.push(0);
        }
        l
    }

    fn key(e: &Elem) -> (String, String, usize, usize, usize, u32) {
        (e.word.clone(), e.ruby.clone(), e.lcid, e.rcid, e.mid, e.base.to_bits())
    }

    fn check_against_uncached(limit: usize, rounds: usize) -> usize {
        let dict = Dictionary::open_default().unwrap();
        let corpus = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tools/kkc_corpus.txt")).unwrap();
        let readings: Vec<&str> = corpus.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("typed-seq")).collect();
        assert!(readings.len() >= 100, "corpus too small");
        let mut cache = LookupCache::with_limit(limit);
        let mut checked = 0;
        for _ in 0..rounds {
            for r in readings.iter().step_by(readings.len() / 80 + 1) {
                let chars: Vec<char> = to_katakana(r).chars().collect();
                for start in 0..chars.len() {
                    let sub: String = chars[start..(start + MAX_LENGTH).min(chars.len())].iter().collect();
                    let want: Vec<_> = dict
                        .prefix_entries(&sub)
                        .unwrap()
                        .into_iter()
                        .map(|e| (e.word, e.ruby, e.lcid as usize, e.rcid as usize, e.mid as usize, e.value.to_bits()))
                        .collect();
                    let got: Vec<_> = cache.lookup(&dict, &sub).iter().flat_map(|g| g.iter()).map(|f| key(&f.elem)).collect();
                    assert_eq!(want, got, "窓 {start} の辞書引きが直接引いた結果と違う (limit {limit})");
                    checked += 1;
                }
            }
        }
        checked
    }

    #[test]
    fn node_cache_lookup_matches_uncached_lookup() {
        assert!(check_against_uncached(CACHE_LIMIT, 2) > 500);
    }

    #[test]
    fn node_cache_lookup_matches_uncached_lookup_when_cleared_often() {
        assert!(check_against_uncached(7, 2) > 500);
    }

    fn totals(l: &Lattice, idx: usize) -> Vec<f32> {
        l.nodes[idx].prevs.iter().map(|&r| l.regs[r as usize].total).collect()
    }

    #[test]
    fn insertion_keeps_descending_order_and_caps_at_n_best() {
        let dict = Dictionary::open_default().unwrap();
        // 3 通りの先頭ノード -> 末尾ノード。CC は辞書の値を使うが、全部同じ品詞なので差は同じ。
        let defs = vec![
            (0, 1, elem("あ", "ア", 1288, 1288, 501, -5.0)),
            (0, 1, elem("亜", "ア", 1288, 1288, 501, -3.0)),
            (0, 1, elem("阿", "ア", 1288, 1288, 501, -4.0)),
            (1, 2, elem("い", "イ", 1288, 1288, 501, -1.0)),
        ];
        let mut l = manual(2, defs);
        l.run(&dict, 2);
        // n_best 2: 上位 2 つ (-3, -4 を含む経路) だけが残る
        let t = totals(&l, 4);
        assert_eq!(t.len(), 2);
        assert!(t[0] > t[1]);
        // EOS は上限なし: 末尾ノード 1 つに prev が 2 つ
        assert_eq!(l.eos.len(), 2);
    }

    #[test]
    fn equal_values_are_inserted_after_existing_ones() {
        let dict = Dictionary::open_default().unwrap();
        let defs = vec![
            (0, 1, elem("A", "ア", 1288, 1288, 501, -3.0)),
            (0, 1, elem("B", "ア", 1288, 1288, 501, -3.0)),
            (0, 1, elem("C", "ア", 1288, 1288, 501, -3.0)),
            (1, 2, elem("い", "イ", 1288, 1288, 501, -1.0)),
        ];
        let mut l = manual(2, defs);
        l.run(&dict, 9);
        // 同値は後から来たものほど後ろ (lastIndex(where: >=) + 1)
        let order: Vec<String> = l.nodes[4]
            .prevs
            .iter()
            .map(|&r| l.nodes[l.regs[r as usize].node as usize].elem.word.clone())
            .collect();
        assert_eq!(order, vec!["A", "B", "C"]);
        // 満杯 (n_best = 2) で同値が来ても入らない
        let defs = vec![
            (0, 1, elem("A", "ア", 1288, 1288, 501, -3.0)),
            (0, 1, elem("B", "ア", 1288, 1288, 501, -3.0)),
            (0, 1, elem("C", "ア", 1288, 1288, 501, -3.0)),
            (1, 2, elem("い", "イ", 1288, 1288, 501, -1.0)),
        ];
        let mut l = manual(2, defs);
        l.run(&dict, 2);
        let order: Vec<String> = l.nodes[4]
            .prevs
            .iter()
            .map(|&r| l.nodes[l.regs[r as usize].node as usize].elem.word.clone())
            .collect();
        assert_eq!(order, vec!["A", "B"]);
    }

    #[test]
    fn removed_nodes_are_neither_expanded_nor_reached() {
        let dict = Dictionary::open_default().unwrap();
        // 値 -20 は threshold (-17) 以下で除かれる
        let defs = vec![
            (0, 1, elem("悪", "ア", 1288, 1288, 501, -20.0)),
            (0, 1, elem("あ", "ア", 1288, 1288, 501, -5.0)),
            (1, 2, elem("い", "イ", 1288, 1288, 501, -1.0)),
        ];
        let mut l = manual(2, defs);
        l.run(&dict, 9);
        assert_eq!(l.eos.len(), 1);
        let words: Vec<String> = l.candidate_data()[0].data.iter().map(|e| e.word.clone()).collect();
        assert_eq!(words, vec!["あ", "い"]);
    }

    #[test]
    fn head_nodes_add_bos_connection_cost_but_others_do_not() {
        let dict = Dictionary::open_default().unwrap();
        let defs = vec![(0, 1, elem("あ", "ア", 1288, 1288, 501, -5.0))];
        let mut l = manual(1, defs);
        l.run(&dict, 9);
        let expect = 0.0 + -5.0 + dict.cc(0, 1288);
        assert_eq!(l.regs[l.eos[0] as usize].total.to_bits(), expect.to_bits());
    }

    #[test]
    fn clause_data_splits_at_content_word_boundaries_and_tracks_mid() {
        let dict = Dictionary::open_default().unwrap();
        // 内容語 (1288) + 助詞 (200: 後置) + 内容語 (1288)
        let defs = vec![
            (0, 1, elem("今日", "キョウ", 1288, 1288, 10, -5.0)),
            (1, 2, elem("は", "ハ", 200, 200, 20, -3.0)),
            (2, 3, elem("晴れ", "ハレ", 1288, 1288, 30, -4.0)),
        ];
        let mut l = manual(3, defs);
        l.run(&dict, 9);
        let cd = &l.candidate_data()[0];
        // 今日+は | 晴れ
        assert_eq!(cd.clauses.len(), 2);
        assert_eq!(cd.clauses[0].0.text, "今日は");
        assert_eq!(cd.clauses[1].0.text, "晴れ");
        assert_eq!(cd.clauses[0].0.data_end_index, 1);
        assert_eq!(cd.clauses[1].0.data_end_index, 2);
        // 文節の mid は最後の内容語 (includeMM を満たす語) の mid
        assert_eq!(cd.clauses[0].0.mid, 10);
        assert_eq!(cd.clauses[1].0.mid, 30);
        assert_eq!(cd.data.len(), 3);
        // 先頭文節の値は「は」の経路 (次の語への連接コストまで含む): 各語の経路の total は遷移先への cc を足したもの
        let first_total = cd.clauses[0].1;
        let at_kyou = -5.0f32 + dict.cc(0, 1288);
        let kyou_reg = dict.cc(1288, 200) + at_kyou;
        let ha_reg = dict.cc(200, 1288) + (kyou_reg + -3.0);
        assert_eq!(first_total.to_bits(), ha_reg.to_bits());
    }
}
