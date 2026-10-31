//! ラティスの結果を候補の並びにまとめる (`KanaKanjiConverter.processResult` と周辺)。
//!
//! 並びは (1) 全文変換の上位 5 件 (3 番目までに読みと一致する候補を保証) (2) 第 1 文節の候補 (最大 5 件)
//! (3) 単語候補 (先頭からの辞書語 + カタカナ・ひらがな・大文字の追加候補。5 件目の位置に特殊候補を挟む)。
//! Swift の `min(count:sortedBy:)` / `sorted` は同点で到着順を保つので、ここでも安定ソートを使う。
#![forbid(unsafe_code)]

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use super::dict::Dictionary;
use super::elem::{canonical_key, Elem, CID_PROPER_NOUN, MID_BOS_EOS, MID_GENERAL};
use super::lattice::{CandidateData, Lattice};
use super::wise::to_hiragana;

/// 候補の出どころ (診断用)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// 全文変換 (上位 5 件)
    Full,
    /// 第 1 文節のみ
    FirstClause,
    /// 先頭からの辞書語
    Word,
    /// 先頭からの `getWiseDicdata` 由来 (数字・記号・1 文字かな)
    Wise,
    /// カタカナ・ひらがな・大文字の追加候補
    Additional,
    /// 特殊候補 (P3 で追加する provider)
    Special,
}

impl Source {
    pub fn tag(self) -> &'static str {
        match self {
            Source::Full => "full",
            Source::FirstClause => "first_clause",
            Source::Word => "word",
            Source::Wise => "wise",
            Source::Additional => "additional",
            Source::Special => "special",
        }
    }
}

/// `Candidate`
#[derive(Debug, Clone)]
pub struct Cand {
    pub text: String,
    pub value: f32,
    /// ルビ文字数 (data の ruby の grapheme 数の合計)
    pub ruby_count: usize,
    pub data: Vec<Rc<Elem>>,
    pub source: Source,
}

impl Cand {
    pub(super) fn new(text: String, value: f32, data: Vec<Rc<Elem>>, source: Source) -> Self {
        let ruby_count = data.iter().map(|e| e.ruby_len).sum();
        Self { text, value, ruby_count, data, source }
    }

    /// data の ruby をつなげたもの
    pub(super) fn ruby(&self) -> String {
        self.data.iter().map(|e| e.ruby.as_str()).collect()
    }
}

/// `processClauseCandidate`: 文節ごとの意味連接コストを足して全文の候補にする。
fn process_clause_candidate(dict: &Dictionary, data: &CandidateData) -> Cand {
    let mut mm_value = 0.0f32;
    let mut mid = MID_BOS_EOS;
    for (clause, _) in &data.clauses {
        mm_value += dict.mm(mid, clause.mid);
        mid = clause.mid;
    }
    let text: String = data.clauses.iter().map(|(c, _)| c.text.as_str()).collect();
    let (_, last_value) = data.clauses.last().expect("clauses は始点の文節を必ず持つ");
    Cand::new(text, *last_value + mm_value, data.data.clone(), Source::Full)
}

/// 第 1 文節だけの候補
fn first_clause_candidate(data: &CandidateData) -> Cand {
    let (first, value) = &data.clauses[0];
    let count = first.data_end_index.max(0) as usize;
    Cand::new(first.text.clone(), *value, data.data[..=count].to_vec(), Source::FirstClause)
}

/// `getUniqueCandidate`: 同じ text は先に現れた位置に、value か ruby_count が大きい方を残す。
fn get_unique(candidates: impl IntoIterator<Item = Cand>, seen: &HashSet<String>) -> Vec<Cand> {
    let mut result: Vec<Cand> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for c in candidates {
        if c.text.is_empty() {
            continue;
        }
        let key = canonical_key(&c.text);
        if seen.contains(&key) {
            continue;
        }
        match index.get(&key) {
            Some(&i) => {
                if result[i].value < c.value || result[i].ruby_count < c.ruby_count {
                    result[i] = c;
                }
            }
            None => {
                index.insert(key, result.len());
                result.push(c);
            }
        }
    }
    result
}

/// `areInIncreasingOrder` で安定ソートする。
fn sort_stable(v: &mut [Cand], less: impl Fn(&Cand, &Cand) -> bool) {
    v.sort_by(|a, b| {
        if less(a, b) {
            Ordering::Less
        } else if less(b, a) {
            Ordering::Greater
        } else {
            Ordering::Equal
        }
    });
}

/// swift-algorithms の `min(count:sortedBy:)`。同点は到着順。
fn min_count(mut v: Vec<Cand>, count: usize, less: impl Fn(&Cand, &Cand) -> bool) -> Vec<Cand> {
    sort_stable(&mut v, less);
    v.truncate(count);
    v
}

fn by_value_desc(a: &Cand, b: &Cand) -> bool {
    a.value > b.value
}

/// ルビが長い順、同じなら value が高い順
fn by_ruby_then_value(a: &Cand, b: &Cand) -> bool {
    if a.ruby_count == b.ruby_count {
        a.value > b.value
    } else {
        a.ruby_count > b.ruby_count
    }
}

/// `getKatakanaScore`: 小さいほどカタカナ語らしい。
fn katakana_score(katakana: &str) -> f32 {
    let mut score = 1.0f32;
    for c in katakana.chars() {
        if "プヴペィフ".contains(c) {
            score *= 0.5;
        } else if "ュピポ".contains(c) {
            score *= 0.6;
        } else if "パォグーム".contains(c) {
            score *= 0.7;
        }
    }
    score
}

/// `getAdditionalCandidate`: 読みをそのままカタカナ・ひらがな・大文字にした候補。
fn additional_candidates(katakana: &str) -> Vec<Cand> {
    let mk = |word: String, data_value: f32, cand_value: f32| {
        let elem = Rc::new(Elem::new(word.clone(), katakana.to_string(), CID_PROPER_NOUN, CID_PROPER_NOUN, MID_GENERAL, data_value));
        Cand::new(word, cand_value, vec![elem], Source::Additional)
    };
    let value = -14.0 * katakana_score(katakana);
    vec![
        mk(katakana.to_string(), value, value),
        mk(to_hiragana(katakana), -14.5, -14.5),
        mk(katakana.to_uppercase(), -15.0, -14.6),
    ]
}

/// 特殊候補。Swift の `defaultSpecialCandidateProviders` のうち、P0 の測定で上位 9 件に現れた
/// メールアドレスと Unicode だけを移植する (calendar / version / time / comma は意図的に移植しない)。
/// 並びは Swift と同じく email -> unicode。
fn special_candidates(reading_katakana: &str) -> Vec<Cand> {
    let mut out = email_address_candidates(reading_katakana);
    out.extend(unicode_candidates(reading_katakana));
    out
}

fn special_cand(text: String, ruby: &str, value: f32) -> Cand {
    let elem = Rc::new(Elem::new(text.clone(), ruby.to_string(), 0, 0, MID_GENERAL, value));
    Cand::new(text, value, vec![elem], Source::Special)
}

const EMAIL_DOMAINS: [&str; 21] = [
    "@gmail.com",
    "@icloud.com",
    "@yahoo.co.jp",
    "@au.com",
    "@docomo.ne.jp",
    "@excite.co.jp",
    "@ezweb.ne.jp",
    "@googlemail.com",
    "@hotmail.co.jp",
    "@hotmail.com",
    "@i.softbank.jp",
    "@live.jp",
    "@me.com",
    "@mineo.jp",
    "@nifty.com",
    "@outlook.com",
    "@outlook.jp",
    "@softbank.ne.jp",
    "@yahoo.ne.jp",
    "@ybb.ne.jp",
    "@ymobile.ne.jp",
];

/// `String.isEnglishSentence`: 空でなく、半角英数と一部の記号だけ
fn is_english_sentence(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "\n !'_<>[]{}*@`^|~=\"#$%&+(),-./:;?\u{2019}\\".contains(c))
}

/// `toEmailAddressCandidates`: 最後の `@` より後ろをドメインの前方一致として、メールアドレス風の候補を出す。
fn email_address_candidates(reading: &str) -> Vec<Cand> {
    let Some(at) = reading.rfind('@') else {
        return Vec::new();
    };
    let id = &reading[..at];
    let domain_prefix = &reading[at + 1..];
    if !(id.is_empty() || is_english_sentence(id)) {
        return Vec::new();
    }
    let base: f32 = if id.is_empty() { -20.0 } else { -13.0 };
    let wanted = format!("@{domain_prefix}");
    EMAIL_DOMAINS
        .iter()
        .enumerate()
        .filter(|(_, d)| d.starts_with(&wanted))
        .map(|(i, d)| special_cand(format!("{id}{d}"), reading, base - i as f32))
        .collect()
}

/// `unicodeCandidates`: `u3042` / `U+3042` などの16進表記を対応する1文字にする。
/// Swift の `Int(_, radix: 16)` と同じく先頭の `+` / `-` は許し、前置詞は `u`, `U`, `u+`, `U+` の順に試す。
fn unicode_candidates(reading: &str) -> Vec<Cand> {
    for prefix in ["u", "U", "u+", "U+"] {
        let Some(rest) = reading.strip_prefix(prefix) else {
            continue;
        };
        let ch = i64::from_str_radix(rest, 16).ok().and_then(|n| u32::try_from(n).ok()).and_then(char::from_u32);
        if let Some(ch) = ch {
            return vec![special_cand(ch.to_string(), reading, -10.0)];
        }
    }
    Vec::new()
}

/// 3 番目までに最低でも 1 つ、(誤り訂正ではなく) 入力に完全一致する候補が入るようにする。
/// 4 番目以降にあればそれを 3 番目に上げ、無ければ全文候補全体から探して挟む。
fn ensure_reading_match(result: &mut Vec<Cand>, whole_sentence: &[Cand], reading_katakana: &str) {
    if result.iter().take(3).any(|c| c.ruby() == reading_katakana) {
        return;
    }
    let found = result.iter().skip(3).position(|c| c.ruby() == reading_katakana).map(|p| p + 3);
    if let Some(i) = found {
        let c = result.remove(i);
        let at = result.len().min(2);
        result.insert(at, c);
    } else if let Some(c) = whole_sentence.iter().find(|c| c.ruby() == reading_katakana) {
        let at = result.len().min(2);
        result.insert(at, c.clone());
    }
}

/// 変換結果。`main` が `mainResults`、`first_clause` が `firstClauseResults`。
#[derive(Debug, Default)]
pub struct Converted {
    pub main: Vec<Cand>,
    pub first_clause: Vec<Cand>,
}

/// `processResult`。`reading_katakana` は読み全体 (カタカナ化済み)。
pub fn process(dict: &Dictionary, lattice: &Lattice, reading_katakana: &str) -> Converted {
    let clause_result = lattice.candidate_data();
    let empty_seen: HashSet<String> = HashSet::new();
    if clause_result.is_empty() {
        let candidates = get_unique(additional_candidates(reading_katakana), &empty_seen);
        return Converted { main: candidates.clone(), first_clause: candidates };
    }

    // 文章全体を変換した場合の候補上位 5 件
    let whole_sentence_unique = get_unique(clause_result.iter().map(|d| process_clause_candidate(dict, d)), &empty_seen);
    let full_candidates = min_count(whole_sentence_unique.clone(), 5, by_value_desc);

    // 文節のみ変換するパターン
    let unique_first_clause = get_unique(clause_result.iter().map(first_clause_candidate), &empty_seen);
    let first_clause_results = min_count(unique_first_clause.clone(), 5, by_ruby_then_value);
    let mut seen: HashSet<String> = full_candidates.iter().map(|c| canonical_key(&c.text)).collect();
    let first_clause_candidates = min_count(get_unique(unique_first_clause, &seen), 5, by_ruby_then_value);
    for c in &first_clause_candidates {
        seen.insert(canonical_key(&c.text));
    }

    // 文字列の長さごとに並べ、その中で評価の高いものから順に並べる
    let word_candidates = {
        let dic_candidates = lattice.nodes_at(0).iter().map(|n| {
            Cand::new(
                n.elem.word.clone(),
                n.elem.value(),
                vec![n.elem.clone()],
                if n.wise { Source::Wise } else { Source::Word },
            )
        });
        let additional = additional_candidates(reading_katakana);
        let mut candidates = get_unique(dic_candidates.chain(additional), &seen);
        sort_stable(&mut candidates, by_ruby_then_value);
        for c in &candidates {
            seen.insert(canonical_key(&c.text));
        }
        // 賢く変換するパターン。途中に挟む
        let wise = get_unique(special_candidates(reading_katakana), &seen);
        let at = candidates.len().min(5);
        candidates.splice(at..at, wise);
        candidates
    };

    let mut result = full_candidates;
    ensure_reading_match(&mut result, &whole_sentence_unique, reading_katakana);
    result.extend(first_clause_candidates);
    result.extend(word_candidates);
    Converted { main: result, first_clause: first_clause_results }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(text: &str, ruby: &str, value: f32) -> Cand {
        let e = Rc::new(Elem::new(text.into(), ruby.into(), 1288, 1288, 501, value));
        Cand::new(text.into(), value, vec![e], Source::Word)
    }

    #[test]
    fn unique_keeps_first_position_and_better_duplicate() {
        let list = vec![cand("a", "ア", -5.0), cand("b", "イ", -4.0), cand("a", "ア", -3.0), cand("b", "イ", -9.0)];
        let u = get_unique(list, &HashSet::new());
        assert_eq!(u.iter().map(|c| (c.text.as_str(), c.value)).collect::<Vec<_>>(), vec![("a", -3.0), ("b", -4.0)]);
    }

    #[test]
    fn unique_prefers_longer_ruby_even_with_lower_value() {
        let list = vec![cand("a", "ア", -3.0), cand("a", "アア", -8.0)];
        let u = get_unique(list, &HashSet::new());
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].ruby_count, 2);
    }

    #[test]
    fn unique_skips_empty_text_and_seen() {
        let seen: HashSet<String> = ["a".to_string()].into_iter().collect();
        let list = vec![cand("", "ア", -1.0), cand("a", "ア", -1.0), cand("c", "ア", -1.0)];
        let u = get_unique(list, &seen);
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].text, "c");
    }

    #[test]
    fn min_count_is_stable_for_ties() {
        let list = vec![cand("a", "ア", -3.0), cand("b", "ア", -3.0), cand("c", "ア", -1.0), cand("d", "ア", -3.0)];
        let top = min_count(list, 3, by_value_desc);
        assert_eq!(top.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(), vec!["c", "a", "b"]);
    }

    #[test]
    fn first_clause_order_prefers_longer_ruby() {
        let a = cand("short", "ア", -1.0);
        let b = cand("long", "アアア", -9.0);
        assert!(by_ruby_then_value(&b, &a));
        assert!(!by_ruby_then_value(&a, &b));
    }

    #[test]
    fn additional_candidates_follow_swift_values() {
        let c = additional_candidates("キョウ");
        assert_eq!(c[0].text, "キョウ");
        assert_eq!(c[0].value, -14.0);
        assert_eq!(c[1].text, "きょう");
        assert_eq!(c[1].value, -14.5);
        assert_eq!(c[2].text, "キョウ");
        assert_eq!(c[2].value, -14.6);
        assert_eq!(c[2].data[0].value(), -15.0);
        // 小さい score: ュ -> 0.6
        assert_eq!(c[0].ruby_count, 3);
        let p = additional_candidates("プ");
        assert_eq!(p[0].value, -14.0 * 0.5);
        let m = additional_candidates("ピュ");
        assert_eq!(m[0].value, -14.0 * (0.6f32 * 0.6f32));
        let u = additional_candidates("abc");
        assert_eq!(u[2].text, "ABC");
    }

    fn special_texts(reading: &str) -> Vec<String> {
        special_candidates(reading).into_iter().map(|c| c.text).collect()
    }

    #[test]
    fn email_bare_at_lists_all_domains_in_order() {
        let c = special_candidates("@");
        assert_eq!(c.len(), 21);
        assert_eq!(c[0].text, "@gmail.com");
        assert_eq!(c[20].text, "@ymobile.ne.jp");
        assert_eq!(c[0].value, -20.0);
        assert_eq!(c[3].value, -23.0);
        assert!(c.iter().all(|x| x.source == Source::Special));
    }

    #[test]
    fn email_with_id_and_prefix_filters_domains() {
        let c = special_candidates("abc@yah");
        assert_eq!(texts(&c), vec!["abc@yahoo.co.jp", "abc@yahoo.ne.jp"]);
        assert_eq!(c[0].value, -13.0 - 2.0);
        assert_eq!(c[1].value, -13.0 - 18.0);
        assert_eq!(special_texts("a@gmail.com"), vec!["a@gmail.com"]);
        assert!(special_texts("a@zzz").is_empty());
    }

    #[test]
    fn email_uses_last_at_and_rejects_non_english_id() {
        assert_eq!(special_texts("a@b@me"), vec!["a@b@me.com"]);
        assert!(special_texts("あ@").is_empty());
        assert!(special_texts("ア@gmail").is_empty());
        assert!(special_texts("abc").is_empty());
    }

    #[test]
    fn unicode_hex_with_each_prefix() {
        assert_eq!(special_texts("u3042"), vec!["あ"]);
        assert_eq!(special_texts("U3042"), vec!["あ"]);
        assert_eq!(special_texts("u+3042"), vec!["あ"]);
        assert_eq!(special_texts("U+1F600"), vec!["\u{1F600}"]);
        let c = special_candidates("u41");
        assert_eq!(c[0].value, -10.0);
        assert_eq!(c[0].data[0].ruby, "u41");
    }

    #[test]
    fn unicode_rejects_invalid_values() {
        assert!(special_texts("u").is_empty());
        assert!(special_texts("u+").is_empty());
        assert!(special_texts("uzz").is_empty());
        assert!(special_texts("ud800").is_empty()); // surrogate
        assert!(special_texts("u110000").is_empty()); // 範囲外
        assert!(special_texts("u-41").is_empty());
        assert!(special_texts("あ41").is_empty());
    }

    #[test]
    fn special_candidates_are_inserted_after_fifth_word_candidate() {
        let core = super::super::KkcCore::open().unwrap();
        let c = core.convert_all("u+3042");
        let w: Vec<&Cand> = c.main.iter().filter(|x| matches!(x.source, Source::Word | Source::Wise | Source::Additional | Source::Special)).collect();
        let pos = w.iter().position(|x| x.source == Source::Special).unwrap();
        assert!(pos <= 5);
        assert_eq!(w[pos].text, "あ");
    }

    #[test]
    fn katakana_score_uses_each_character_once() {
        assert_eq!(katakana_score("アイウ"), 1.0);
        assert_eq!(katakana_score("パ"), 0.7);
        assert_eq!(katakana_score("プ"), 0.5);
    }

    fn texts(v: &[Cand]) -> Vec<&str> {
        v.iter().map(|c| c.text.as_str()).collect()
    }

    #[test]
    fn reading_match_is_kept_when_already_in_top3() {
        let mut r = vec![cand("x", "イ", -1.0), cand("y", "アイ", -2.0), cand("z", "ウ", -3.0)];
        let before = texts(&r).join(",");
        ensure_reading_match(&mut r, &[], "アイ");
        assert_eq!(texts(&r).join(","), before);
    }

    #[test]
    fn reading_match_in_4th_or_later_moves_to_third() {
        let mut r = vec![cand("a", "イ", -1.0), cand("b", "イ", -2.0), cand("c", "イ", -3.0), cand("d", "イ", -4.0), cand("e", "アイ", -5.0)];
        ensure_reading_match(&mut r, &[], "アイ");
        assert_eq!(texts(&r), vec!["a", "b", "e", "c", "d"]);
    }

    #[test]
    fn missing_reading_match_is_taken_from_whole_sentence() {
        let mut r = vec![cand("a", "イ", -1.0), cand("b", "イ", -2.0), cand("c", "イ", -3.0)];
        ensure_reading_match(&mut r, &[cand("m", "アイ", -9.0)], "アイ");
        assert_eq!(texts(&r), vec!["a", "b", "m", "c"]);
        let mut r = vec![cand("a", "イ", -1.0)];
        ensure_reading_match(&mut r, &[cand("w", "アイ", -9.0)], "アイ");
        // 1 件しかないときは末尾 (min(endIndex, 2))
        assert_eq!(texts(&r), vec!["a", "w"]);
        // どこにも無ければ何もしない
        let mut r = vec![cand("a", "イ", -1.0)];
        ensure_reading_match(&mut r, &[], "アイ");
        assert_eq!(texts(&r), vec!["a"]);
    }

    #[test]
    fn real_dictionary_conversion_matches_swift_golden_top() {
        let core = super::super::KkcCore::open().unwrap();
        let c = core.convert_all("きょうはいいてんきですね");
        // Swift 版 (golden) の 1 位とビット一致
        assert_eq!(c.main[0].text, "強はいい天気ですね");
        assert_eq!(c.main[0].value.to_bits(), 0xc21825e3);
        // 追加候補 (カタカナ・ひらがな) は第 1 文節の候補より後ろにある
        let pos = |t: &str| c.main.iter().position(|x| x.text == t).unwrap();
        assert!(pos("今日は") < pos("キョウハイイテンキデスネ"));
        assert_eq!(c.main[pos("キョウハイイテンキデスネ")].source, Source::Additional);
    }

    #[test]
    fn empty_lattice_result_returns_only_additional_candidates() {
        let dict = Dictionary::open_default().unwrap();
        // ノードが無い (EOS に繋がらない) ラティス
        let mut cache = super::super::lattice::LookupCache::new();
        let mut l = Lattice::build(&dict, &mut cache, &super::super::learning::Learning::disabled(), &[]);
        l.run(&dict, 9);
        let r = process(&dict, &l, "キョウ");
        let texts: Vec<&str> = r.main.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["キョウ", "きょう"]);
    }
}
