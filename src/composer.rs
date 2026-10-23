#![forbid(unsafe_code)]

// composer.rs - 未確定文の状態機械: 入力単位(ローマ字+かな) + 同期変換候補 + フェーズ。
// 候補は {text, source} で持ち、リランキング後も学習には Source::Engine の元の順位を渡す。
// ローマ字そのまま(raw)など engine 以外の候補は順位を持たず、確定しても学習しない。

use crate::english_split;
use crate::romaji_converter::Flush;
use crate::symbols;
use crate::translit::{self, Form};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub text: String,
    pub source: Source,
}

/// `CandidateEngine::convert` が返した順位。学習に渡す唯一の添字。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineRank(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Engine(EngineRank),
    /// 記号 1 つの異体候補。
    Symbol,
    Raw,
    Hiragana,
    Katakana,
    /// カタカナ語に対応する英単語。
    Loanword,
    Emoji,
    /// かな混じり (Mixed) で表示中の読みそのもの。
    MixedReading,
}

impl Candidate {
    fn engine_rank(&self) -> Option<EngineRank> {
        match self.source {
            Source::Engine(rank) => Some(rank),
            _ => None,
        }
    }
}

pub trait CandidateEngine {
    fn convert(&mut self, revision: u64, hiragana: &str) -> Vec<String>;
    fn select(&mut self, revision: u64, engine_index: usize) -> bool;
    fn cancel(&mut self);
}

/// 未確定文の局面。Empty は未確定文が無い状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Empty,
    /// かなを入力中(未変換)。
    Composing,
    /// 変換を取り消して、かなをそのまま表示している。
    Reverted,
    /// 候補を選択中。window は候補ウィンドウを出している間 true。
    Converting { selected: usize, window: bool },
    /// 英単語をそのまま入力中(かな変換しない)。
    Literal,
    /// かな入力の途中で大文字を打ち、英単語を続けている局面。スペースでは raw と並べて変換できる。
    Mixed,
    /// ローマ字の末尾が変換しきれず、組成全体を英単語として扱う局面。確定は raw、変換は raw が第 1 候補。
    Dangling,
    /// 字種変換(カタカナ・全角英数など)の結果を表示している。確定は表示中の文字列で、学習しない。
    Transliterated(Form),
}

/// 1 回の入力で生まれた (打鍵, かな) の組。部分削除後は raw をかなで置き換える。
#[derive(Debug, Clone)]
struct Unit {
    raw: String,
    kana: String,
    /// 大文字から始めた英単語など、かな変換せずそのまま入れた部分。
    literal: bool,
}

/// 1 回の変換で入れる絵文字の上限
const EMOJI_LIMIT: usize = 5;
/// 絵文字より前に並べるエンジン候補の数
const EMOJI_AFTER_ENGINE: usize = 3;

pub struct Composer<E: CandidateEngine> {
    engine: E,
    units: Vec<Unit>,
    pending_raw: String,
    hiragana: String,
    candidates: Vec<Candidate>,
    phase: Phase,
    live: bool,
    generation: u64,
    pending_rerank: Option<usize>,
    translit: String,
    /// 字種変換に入る前の局面。スペースで変換に戻るときに使う。
    base: Phase,
    /// カタカナ語に対応する英単語を引く関数。テストでは差し替える。
    english_for: fn(&str) -> Vec<String>,
    /// 打鍵列が英単語かを判定する関数。テストでは差し替える。
    is_english_word: fn(&str) -> bool,
    /// 英語として確定したことのある語か (english_memory)。テストでは差し替える。
    knows_english: fn(&str) -> bool,
    /// 英語として確定した語を覚える関数。
    learn_english: fn(&str),
    /// ひらがなの読み全体に対応する絵文字を引く関数。テストでは差し替える。
    emoji_for: fn(&str) -> Vec<String>,
}

impl<E: CandidateEngine> Composer<E> {
    pub fn new(engine: E) -> Self {
        Self {
            engine,
            units: Vec::new(),
            pending_raw: String::new(),
            hiragana: String::new(),
            candidates: Vec::new(),
            phase: Phase::Empty,
            live: false,
            generation: 0,
            pending_rerank: None,
            translit: String::new(),
            base: Phase::Empty,
            english_for: |_| Vec::new(),
            is_english_word: |_| false,
            knows_english: |_| false,
            learn_english: |_| {},
            emoji_for: |_| Vec::new(),
        }
    }

    /// カタカナ語の英単語候補 (desktop など) を引く関数を差し込む。
    pub fn with_english(mut self, english_for: fn(&str) -> Vec<String>) -> Self {
        self.english_for = english_for;
        self
    }

    /// 打鍵列が英単語かを判定する関数 (enter など) を差し込む。
    pub fn with_english_word(mut self, is_english_word: fn(&str) -> bool) -> Self {
        self.is_english_word = is_english_word;
        self
    }

    /// 英語として確定した語の記憶 (git など) を差し込む。
    pub fn with_english_memory(mut self, knows: fn(&str) -> bool, learn: fn(&str)) -> Self {
        self.knows_english = knows;
        self.learn_english = learn;
        self
    }

    /// 読みに対応する絵文字を引く関数を差し込む。
    pub fn with_emoji(mut self, emoji_for: fn(&str) -> Vec<String>) -> Self {
        self.emoji_for = emoji_for;
        self
    }

    /// 世代番号の開始値。
    pub fn with_generation(mut self, generation: u64) -> Self {
        self.generation = generation;
        self
    }

    pub fn set_live(&mut self, live: bool) {
        self.live = live;
    }

    pub fn is_live(&self) -> bool {
        self.live
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn is_empty(&self) -> bool {
        self.hiragana.is_empty()
    }

    pub fn current_generation(&self) -> u64 {
        self.generation
    }

    pub fn candidates(&self) -> &[Candidate] {
        &self.candidates
    }

    pub fn display_hiragana(&self) -> &str {
        &self.hiragana
    }

    /// 打鍵したままの ASCII 文字列(かなに変換済みの分と未変換の分)。
    pub fn raw(&self) -> String {
        self.units
            .iter()
            .map(|u| u.raw.as_str())
            .chain(std::iter::once(self.pending_raw.as_str()))
            .collect()
    }

    pub fn display(&self) -> &str {
        match self.phase {
            Phase::Converting { selected, .. } => &self.candidates[selected].text,
            Phase::Composing if self.live => self
                .candidates
                .first()
                .map_or(self.hiragana.as_str(), |c| c.text.as_str()),
            Phase::Transliterated(_) => &self.translit,
            _ => &self.hiragana,
        }
    }

    /// 組成全体を form の字種で表示する。直前も同じ種類の字種変換なら、英数形は大文字小文字を巡らせる。
    pub fn transliterate(&mut self, requested: Form) -> bool {
        if self.hiragana.is_empty() {
            return false;
        }
        let current = match self.phase {
            Phase::Transliterated(form) => Some(form),
            _ => None,
        };
        if current.is_none() {
            self.base = match self.phase {
                Phase::Converting { .. } | Phase::Reverted => Phase::Composing,
                other => other,
            };
        }
        let requested = translit::as_typed_first(requested, &self.raw());
        let form = translit::next_form(current, requested);
        self.translit = translit::render(form, &self.hiragana, &self.raw());
        self.phase = Phase::Transliterated(form);
        true
    }

    fn sync_hiragana(&mut self) {
        self.hiragana = self.units.iter().map(|u| u.kana.as_str()).collect();
    }

    fn reset_candidates(&mut self) {
        self.generation += 1;
        self.candidates.clear();
        self.pending_rerank = None;
    }

    fn refresh(&mut self) {
        self.reset_candidates();
        self.sync_hiragana();
        if self.hiragana.is_empty() {
            self.phase = Phase::Empty;
            return;
        }
        self.phase = Phase::Composing;
        if let Some(forms) = self.symbol_variants() {
            self.candidates = forms
                .iter()
                .map(|form| Candidate {
                    text: (*form).to_string(),
                    source: Source::Symbol,
                })
                .collect();
            return;
        }
        let texts = self.engine.convert(self.generation, &self.hiragana);
        for (engine_index, text) in texts.into_iter().enumerate() {
            if !self.candidates.iter().any(|c| c.text == text) {
                self.candidates.push(Candidate {
                    text,
                    source: Source::Engine(EngineRank(engine_index)),
                });
            }
        }
    }

    /// 組成全体が記号 1 つだけのときの異体候補。かなと混ざれば None(辞書変換に任せる)。
    fn symbol_variants(&self) -> Option<&'static [&'static str]> {
        match self.units.as_slice() {
            [_] => symbols::variants(&self.hiragana),
            _ => None,
        }
    }

    /// 組成全体が記号 1 つだけか。LLM の並べ替えや学習の対象にしない。
    pub fn is_symbol(&self) -> bool {
        self.symbol_variants().is_some()
    }

    /// ローマ字バッファの確定結果を取り込む。Dangling なら局面を Dangling にする。
    pub fn push_flush(&mut self, flush: Flush) {
        let english = self.is_typing_english();
        match flush {
            Flush::Empty => {}
            Flush::Kana(kana) | Flush::Dangling(kana) => self.push_input("", &kana, ""),
        }
        if let Some(split) = self.english_split(true) {
            self.fix_english_split(split);
            return;
        }
        if self.phase == Phase::Composing && (english || self.has_ascii_kana()) {
            self.phase = Phase::Dangling;
        }
    }

    /// 英単語のあとに助詞などが続く組成 (artifactni → artifact + に)。finished は打ち終えたとき。
    pub fn english_split(&self, finished: bool) -> Option<english_split::Split> {
        if self.phase != Phase::Composing {
            return None;
        }
        english_split::split(&self.raw(), finished, self.knows_english)
    }

    /// 英単語を 1 つの単位にまとめ、かな混じり (Mixed) として確定・変換できるようにする。
    /// 変換候補は、かなの部分をエンジンで変換したものに英単語を前に付けたもの (artifactを使う)。
    fn fix_english_split(&mut self, split: english_split::Split) {
        let tail_kana: String = split.tail.iter().map(|(_, kana)| kana.as_str()).collect();
        let head = Unit { kana: split.head.clone(), raw: split.head.clone(), literal: true };
        let tail = split.tail.into_iter().map(|(raw, kana)| Unit { raw, kana, literal: false });
        self.units = std::iter::once(head).chain(tail).collect();
        self.pending_raw.clear();
        self.sync_hiragana();
        self.reset_candidates();
        self.phase = Phase::Mixed;
        let texts = self.engine.convert(self.generation, &tail_kana);
        for (engine_index, text) in texts.into_iter().enumerate() {
            let text = format!("{}{text}", split.head);
            if !self.candidates.iter().any(|c| c.text == text) {
                self.candidates.push(Candidate { text, source: Source::Engine(EngineRank(engine_index)) });
            }
        }
    }

    /// 英語を打っていると見なせる局面。英字のまま表示し、確定も英字にする。
    /// かなの途中に変換できない子音が挟まった (merge の めrげ) か、かなにしきれない打鍵が残り、
    /// 打鍵列全体が英語として確定したことのある語 (git) か 4 文字以上の英単語 (enter) になっている。
    /// 表の短い語は日本語の打ちかけ (in, den) と紛れるので外す。
    pub fn is_typing_english(&self) -> bool {
        const MIN_LEN: usize = 4;
        let raw = self.raw();
        let word = || {
            !self.pending_raw.is_empty()
                && ((self.knows_english)(&raw) || (raw.len() >= MIN_LEN && (self.is_english_word)(&raw)))
        };
        self.phase == Phase::Composing
            && raw.bytes().all(|b| b.is_ascii_lowercase())
            && (self.has_ascii_kana() || word())
    }

    /// 変換できずに ASCII のまま残った文字が組成に含まれるか(英単語の合図)。
    fn has_ascii_kana(&self) -> bool {
        self.units
            .iter()
            .any(|u| u.kana.chars().any(|c| c.is_ascii_alphabetic()))
    }

    /// 打鍵 raw と、それで確定したかな kana を追加する。kana が空なら raw は次のかなに持ち越す。
    /// carry はローマ字バッファに残っている未確定の打鍵で、確定した単位には含めず持ち越す。
    pub fn push_input(&mut self, raw: &str, kana: &str, carry: &str) {
        self.pending_raw.push_str(raw);
        if kana.is_empty() {
            return;
        }
        let split = self
            .pending_raw
            .strip_suffix(carry)
            .map_or(self.pending_raw.len(), str::len);
        let carried = self.pending_raw.split_off(split);
        self.units.push(Unit {
            raw: std::mem::replace(&mut self.pending_raw, carried),
            kana: kana.to_string(),
            literal: false,
        });
        self.refresh();
    }

    /// 英単語としてそのまま追加する。かな変換も候補生成もしない。
    pub fn push_literal(&mut self, text: &str) {
        if !self.candidates.is_empty() {
            self.engine.cancel();
        }
        let raw = format!("{}{}", std::mem::take(&mut self.pending_raw), text);
        self.units.push(Unit {
            raw,
            kana: text.to_string(),
            literal: true,
        });
        self.sync_hiragana();
        self.reset_candidates();
        self.phase = if self.units.iter().all(|u| u.literal) {
            Phase::Literal
        } else {
            Phase::Mixed
        };
    }

    pub fn pop_pending_raw(&mut self) {
        self.pending_raw.pop();
    }

    pub fn clear_pending_raw(&mut self) {
        self.pending_raw.clear();
    }

    pub fn backspace(&mut self) -> bool {
        let Some(last) = self.units.last_mut() else {
            return false;
        };
        last.kana.pop();
        if last.kana.is_empty() {
            self.units.pop();
        } else {
            last.raw = last.kana.clone();
        }
        let is_literal = |phase| matches!(phase, Phase::Literal | Phase::Mixed);
        let was_literal = is_literal(self.phase)
            || (matches!(self.phase, Phase::Transliterated(_)) && is_literal(self.base));
        // 英単語の部分を消しきったら、残ったかなは通常の組成に戻す。
        if was_literal && self.units.iter().any(|u| u.literal) {
            self.sync_hiragana();
            self.reset_candidates();
        } else {
            self.refresh();
        }
        true
    }

    /// 最初の変換。保留中のリランキングを反映し、raw 候補を第 1 候補の直後に入れて先頭を選ぶ。
    pub fn begin_conversion(&mut self) -> bool {
        if matches!(self.phase, Phase::Transliterated(_)) {
            self.phase = self.base;
        }
        if !matches!(
            self.phase,
            Phase::Composing | Phase::Reverted | Phase::Dangling | Phase::Mixed
        ) {
            return false;
        }
        if let Some(index) = self.pending_rerank.take() {
            self.promote(index);
        }
        if self.phase == Phase::Mixed && !self.candidates.iter().any(|c| c.text == self.hiragana) {
            // 表示中のかな混じりを第 1 候補にし、raw はその直後に並べる。
            self.candidates.push(Candidate {
                text: self.hiragana.clone(),
                source: Source::MixedReading,
            });
        }
        self.insert_kana_fallback();
        // 英単語は raw より前に並べたいので、先に raw を入れてから第 1 候補の直後へ割り込ませる。
        self.insert_raw_candidate();
        self.insert_english_candidates();
        self.insert_emoji_candidates();
        if self.candidates.is_empty() {
            return false;
        }
        self.phase = Phase::Converting {
            selected: 0,
            window: false,
        };
        true
    }

    /// 次の候補へ進み(末尾なら先頭へ戻り)、候補ウィンドウを出す状態にする。
    pub fn advance(&mut self) -> bool {
        let Phase::Converting { selected, .. } = self.phase else {
            return false;
        };
        self.phase = Phase::Converting {
            selected: (selected + 1) % self.candidates.len(),
            window: true,
        };
        true
    }

    /// 前の候補へ戻り(先頭なら末尾へ回り)、候補ウィンドウを出す状態にする。
    pub fn retreat(&mut self) -> bool {
        let Phase::Converting { selected, .. } = self.phase else {
            return false;
        };
        self.phase = Phase::Converting {
            selected: (selected + self.candidates.len() - 1) % self.candidates.len(),
            window: true,
        };
        true
    }

    pub fn select_candidate(&mut self, index: usize) -> bool {
        if index >= self.candidates.len() {
            return false;
        }
        self.phase = Phase::Converting {
            selected: index,
            window: true,
        };
        true
    }

    pub fn select_candidate_text(&mut self, text: &str) -> bool {
        match self.candidates.iter().position(|c| c.text == text) {
            Some(index) => self.select_candidate(index),
            None => false,
        }
    }

    /// 組成を捨てて空に戻す(何も確定しない)。
    pub fn discard(&mut self) {
        self.engine.cancel();
        self.units.clear();
        self.pending_raw.clear();
        self.hiragana.clear();
        self.reset_candidates();
        self.phase = Phase::Empty;
    }

    pub fn revert(&mut self) {
        if self.phase != Phase::Empty {
            self.phase = Phase::Reverted;
        }
    }

    pub fn commit(&mut self, learn: bool) -> String {
        let out = match self.phase {
            Phase::Dangling => self.raw(),
            _ => self.display().to_string(),
        };
        if learn && self.phase == Phase::Dangling {
            (self.learn_english)(&out);
        }
        for unit in self.units.iter().filter(|u| u.literal && learn) {
            (self.learn_english)(&unit.raw);
        }
        let learned = match self.phase {
            Phase::Converting { selected, .. } => self.candidates[selected].engine_rank(),
            Phase::Composing if self.live => self.candidates.first().and_then(Candidate::engine_rank),
            _ => None,
        };
        match learned.filter(|_| learn) {
            Some(EngineRank(i)) => {
                let _ = self.engine.select(self.generation, i);
            }
            None => self.engine.cancel(),
        }
        self.units.clear();
        self.pending_raw.clear();
        self.hiragana.clear();
        self.reset_candidates();
        self.phase = Phase::Empty;
        out
    }

    /// LLM が選んだ表示位置 display_index を先頭へ昇格する。display_index は要求時の候補列(raw なし)での位置。
    /// live なら即反映して新しい表示を返す。非 live では保留し、最初の変換時に反映する。
    /// 変換済みでも、まだ送りも選択もしていない(先頭選択でウィンドウなし)間は反映する。
    pub fn apply_rerank(&mut self, generation: u64, display_index: usize) -> Option<String> {
        if generation != self.generation || display_index == 0 {
            return None;
        }
        match self.phase {
            Phase::Composing if display_index < self.candidates.len() => {
                if self.live {
                    self.promote(display_index);
                    Some(self.display().to_string())
                } else {
                    self.pending_rerank = Some(display_index);
                    None
                }
            }
            Phase::Converting {
                selected: 0,
                window: false,
            } => self.promote_converting(display_index),
            _ => None,
        }
    }

    pub fn has_pending_rerank(&self) -> bool {
        self.pending_rerank.is_some()
    }

    /// 変換済み候補列から raw・英単語・絵文字(engine 以外)を一旦外して display_index 番目を先頭へ昇格し、
    /// raw・英単語は第 1 候補の直後へ、絵文字は元の位置規則どおりに戻す。
    fn promote_converting(&mut self, display_index: usize) -> Option<String> {
        let (engine, extras): (Vec<Candidate>, Vec<Candidate>) = std::mem::take(&mut self.candidates)
            .into_iter()
            .partition(|c| c.engine_rank().is_some());
        let (emoji, extras): (Vec<Candidate>, Vec<Candidate>) = extras
            .into_iter()
            .partition(|c| c.source == Source::Emoji);
        self.candidates = engine;
        let promoted = display_index < self.candidates.len();
        if promoted {
            self.promote(display_index);
        }
        // 外す前の並びは復元できないので、第 1 候補の直後へ戻す。
        let position = self.candidates.len().min(1);
        self.candidates.splice(position..position, extras);
        self.place_emoji(emoji);
        promoted.then(|| self.display().to_string())
    }

    fn promote(&mut self, display_index: usize) {
        if display_index < self.candidates.len() {
            let chosen = self.candidates.remove(display_index);
            self.candidates.insert(0, chosen);
        }
    }

    /// 組成全体がカタカナ語として引ける場合、英単語を第 1 候補の直後に入れる (raw はその後ろ)。
    fn insert_english_candidates(&mut self) {
        if matches!(self.phase, Phase::Dangling | Phase::Mixed) || self.is_symbol() {
            return;
        }
        let words = (self.english_for)(&translit::to_katakana(&self.hiragana));
        let fresh: Vec<Candidate> = words
            .into_iter()
            .filter(|w| !self.candidates.iter().any(|c| &c.text == w))
            .map(|text| Candidate {
                text,
                source: Source::Loanword,
            })
            .collect();
        let position = self.candidates.len().min(1);
        self.candidates.splice(position..position, fresh);
    }

    /// 組成全体が絵文字の読みと一致する場合、絵文字を最大 EMOJI_LIMIT 件入れる。
    fn insert_emoji_candidates(&mut self) {
        if matches!(self.phase, Phase::Dangling | Phase::Mixed) || self.is_symbol() {
            return;
        }
        let fresh: Vec<Candidate> = (self.emoji_for)(&self.hiragana)
            .into_iter()
            .filter(|e| !self.candidates.iter().any(|c| &c.text == e))
            .take(EMOJI_LIMIT)
            .map(|text| Candidate {
                text,
                source: Source::Emoji,
            })
            .collect();
        self.place_emoji(fresh);
    }

    /// 絵文字を、上位 3 件のエンジン候補と英単語・raw の後ろに入れる (3 番目より前には入れない)。
    fn place_emoji(&mut self, emoji: Vec<Candidate>) {
        let engine = self.candidates.iter().filter(|c| c.engine_rank().is_some()).count();
        let extras = self.candidates.len() - engine;
        let position = (extras + engine.min(EMOJI_AFTER_ENGINE)).max(3).min(self.candidates.len());
        self.candidates.splice(position..position, emoji);
    }

    /// エンジンが候補を 1 つも返さなかったとき (辞書が開けない・未知の読みなど) に、ひらがなとカタカナを候補にする。
    /// これがないと raw (ローマ字) が唯一の候補になり、Space+Enter で日本語がローマ字に化ける。
    fn insert_kana_fallback(&mut self) {
        let unconverted = self.candidates.iter().all(|c| c.engine_rank().is_none());
        if !unconverted || self.is_symbol() || !matches!(self.phase, Phase::Composing | Phase::Reverted) {
            return;
        }
        let forms = [
            (self.hiragana.clone(), Source::Hiragana),
            (translit::to_katakana(&self.hiragana), Source::Katakana),
        ];
        for (text, source) in forms {
            if !text.is_empty() && !self.candidates.iter().any(|c| c.text == text) {
                self.candidates.push(Candidate { text, source });
            }
        }
    }

    fn insert_raw_candidate(&mut self) {
        let raw = self.raw();
        if raw.is_empty() || self.is_symbol() || self.candidates.iter().any(|c| c.text == raw) {
            return;
        }
        let position = match self.phase {
            Phase::Dangling => 0,
            _ => self.candidates.len().min(1),
        };
        self.candidates.insert(
            position,
            Candidate {
                text: raw,
                source: Source::Raw,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Clone, Default)]
    struct Log {
        converts: Rc<RefCell<Vec<(u64, String)>>>,
        selects: Rc<RefCell<Vec<(u64, usize)>>>,
        cancels: Rc<RefCell<usize>>,
    }

    struct MockEngine {
        log: Log,
        table: Vec<(String, Vec<String>)>,
    }

    impl CandidateEngine for MockEngine {
        fn convert(&mut self, revision: u64, hiragana: &str) -> Vec<String> {
            self.log
                .converts
                .borrow_mut()
                .push((revision, hiragana.to_string()));
            for (k, v) in &self.table {
                if k == hiragana {
                    return v.clone();
                }
            }
            vec![format!("{}K1", hiragana), format!("{}K2", hiragana)]
        }
        fn select(&mut self, revision: u64, engine_index: usize) -> bool {
            self.log.selects.borrow_mut().push((revision, engine_index));
            true
        }
        fn cancel(&mut self) {
            *self.log.cancels.borrow_mut() += 1;
        }
    }

    impl Composer<MockEngine> {
        fn engine_cancels(&self) -> std::cell::Ref<'_, usize> {
            self.engine.log.cancels.borrow()
        }
    }

    fn composer() -> (Composer<MockEngine>, Log) {
        let log = Log::default();
        let engine = MockEngine {
            log: log.clone(),
            table: vec![(
                "きょう".to_string(),
                vec!["今日".to_string(), "京".to_string(), "教".to_string()],
            )],
        };
        (Composer::new(engine), log)
    }

    fn type_kyou(c: &mut Composer<MockEngine>) {
        c.push_input("k", "", "");
        c.push_input("yo", "きょ", "");
        c.push_input("u", "う", "");
    }

    fn texts(c: &Composer<MockEngine>) -> Vec<&str> {
        c.candidates().iter().map(|x| x.text.as_str()).collect()
    }

    #[test]
    fn 入力なしでは空() {
        let (c, _) = composer();
        assert!(c.is_empty());
        assert_eq!(c.phase(), Phase::Empty);
        assert_eq!(c.display(), "");
    }

    #[test]
    fn 既定は非liveで生ひらがなを表示() {
        let (mut c, log) = composer();
        type_kyou(&mut c);
        assert_eq!(c.phase(), Phase::Composing);
        assert_eq!(c.display(), "きょう");
        assert_eq!(c.raw(), "kyou");
        assert_eq!(log.converts.borrow().len(), 2);
    }

    #[test]
    fn liveなら第1候補を表示() {
        let (mut c, _) = composer();
        c.set_live(true);
        type_kyou(&mut c);
        assert_eq!(c.display(), "今日");
    }

    #[test]
    fn 最初の変換で第1候補を選び_候補ウィンドウはまだ出さない() {
        let (mut c, _) = composer();
        type_kyou(&mut c);
        assert!(c.begin_conversion());
        assert_eq!(
            c.phase(),
            Phase::Converting {
                selected: 0,
                window: false
            }
        );
        assert_eq!(c.display(), "今日");
    }

    #[test]
    fn 変換後の送りで候補を順に進めwrapし_ウィンドウを出す() {
        let (mut c, _) = composer();
        type_kyou(&mut c);
        c.begin_conversion();
        assert!(c.advance());
        assert_eq!(
            c.phase(),
            Phase::Converting {
                selected: 1,
                window: true
            }
        );
        assert_eq!(c.display(), "kyou");
        c.advance();
        c.advance();
        assert_eq!(c.display(), "教");
        c.advance();
        assert_eq!(c.display(), "今日");
    }

    #[test]
    fn 戻りは候補を逆順に進め_先頭から末尾へwrapする() {
        let (mut c, _) = composer();
        type_kyou(&mut c);
        c.begin_conversion();
        assert!(c.retreat());
        assert_eq!(c.display(), "教");
        assert_eq!(
            c.phase(),
            Phase::Converting {
                selected: c.candidates().len() - 1,
                window: true
            }
        );
        assert!(c.advance());
        assert_eq!(c.display(), "今日");
        c.advance();
        assert!(c.retreat());
        assert_eq!(c.display(), "今日");
    }

    #[test]
    fn 変換前は戻れない() {
        let (mut c, _) = composer();
        assert!(!c.retreat());
        type_kyou(&mut c);
        assert!(!c.retreat());
    }

    #[test]
    fn 変換前は送れない() {
        let (mut c, _) = composer();
        assert!(!c.begin_conversion());
        assert!(!c.advance());
        type_kyou(&mut c);
        assert!(!c.advance());
    }

    #[test]
    fn raw候補は第1候補の直後に一度だけ入る() {
        let (mut c, _) = composer();
        type_kyou(&mut c);
        c.begin_conversion();
        assert_eq!(texts(&c), vec!["今日", "kyou", "京", "教"]);
        c.revert();
        assert!(c.begin_conversion());
        assert_eq!(texts(&c), vec!["今日", "kyou", "京", "教"]);
    }

    #[test]
    fn raw候補は同じ文字列の候補があれば重複しない() {
        let log = Log::default();
        let engine = MockEngine {
            log,
            table: vec![("ぎっとはぶ".to_string(), vec!["github".to_string()])],
        };
        let mut c = Composer::new(engine);
        c.push_input("github", "ぎっとはぶ", "");
        c.begin_conversion();
        assert_eq!(texts(&c), vec!["github"]);
        assert_eq!(c.candidates()[0].engine_rank(), Some(EngineRank(0)));
    }

    #[test]
    fn 変換候補が空ならひらがなとカタカナが候補になりrawは先頭にならない() {
        let log = Log::default();
        let engine = MockEngine {
            log,
            table: vec![("ぎ".to_string(), vec![])],
        };
        let mut c = Composer::new(engine);
        c.push_input("gi", "ぎ", "");
        assert!(c.begin_conversion());
        assert_eq!(c.display(), "ぎ");
        assert_eq!(texts(&c), vec!["ぎ", "gi", "ギ"]);
        assert!(c.candidates().iter().all(|cand| cand.engine_rank().is_none()));
        assert_eq!(c.commit(true), "ぎ");
        assert_eq!(*c.engine_cancels(), 1, "学習しない");
    }

    #[test]
    fn 変換候補が空でもdanglingならrawが先頭のまま() {
        let engine = MockEngine { log: Log::default(), table: vec![] };
        let mut c = Composer::new(engine);
        c.push_flush(Flush::Dangling("きょうh".to_string()));
        c.push_input("kyouh", "", "");
        assert_eq!(c.phase(), Phase::Dangling);
        assert!(c.begin_conversion());
        assert_eq!(c.display(), "kyouh");
    }

    #[test]
    fn 打鍵のみで未変換のローマ字もrawに含まれる() {
        let (mut c, _) = composer();
        c.push_input("gi", "ぎ", "");
        c.push_input("b", "", "");
        assert_eq!(c.raw(), "gib");
        c.pop_pending_raw();
        assert_eq!(c.raw(), "gi");
    }

    #[test]
    fn 入力すると変換が解除され候補が更新される() {
        let (mut c, log) = composer();
        type_kyou(&mut c);
        c.begin_conversion();
        c.advance();
        c.push_input("ha", "は", "");
        assert_eq!(c.phase(), Phase::Composing);
        assert_eq!(c.display(), "きょうは");
        assert_eq!(log.converts.borrow().last().unwrap().1, "きょうは");
    }

    #[test]
    fn 同一テキスト候補はdedupeされ先頭のengine順位が残る() {
        let log = Log::default();
        let engine = MockEngine {
            log: log.clone(),
            table: vec![(
                "きょう".to_string(),
                vec!["今日".to_string(), "京".to_string(), "今日".to_string()],
            )],
        };
        let mut c = Composer::new(engine);
        type_kyou(&mut c);
        assert_eq!(c.candidates().len(), 2);
        assert_eq!(c.candidates()[0].text, "今日");
        assert_eq!(c.candidates()[0].engine_rank(), Some(EngineRank(0)));
        assert_eq!(c.candidates()[1].text, "京");
        assert_eq!(c.candidates()[1].engine_rank(), Some(EngineRank(1)));
    }

    #[test]
    fn backspaceで候補再計算_空になったら候補もクリア() {
        let (mut c, _) = composer();
        type_kyou(&mut c);
        assert!(c.backspace());
        assert_eq!(c.display(), "きょ");
        c.backspace();
        c.backspace();
        assert!(c.is_empty());
        assert_eq!(c.phase(), Phase::Empty);
        assert_eq!(c.display(), "");
        assert!(c.candidates().is_empty());
        assert_eq!(c.raw(), "");
        assert!(!c.backspace());
    }

    #[test]
    fn backspaceで単位ごと消えたかなの打鍵もrawから消える() {
        let (mut c, _) = composer();
        c.push_input("ka", "か", "");
        c.push_input("n", "ん", "");
        c.backspace();
        assert_eq!(c.raw(), "ka");
    }

    #[test]
    fn 変換中のbackspaceは未変換に戻る() {
        let (mut c, _) = composer();
        type_kyou(&mut c);
        c.begin_conversion();
        assert!(c.backspace());
        assert_eq!(c.phase(), Phase::Composing);
        assert_eq!(c.display(), "きょ");
    }

    #[test]
    fn revertで生ひらがな表示_次の入力で解除() {
        let (mut c, _) = composer();
        c.set_live(true);
        type_kyou(&mut c);
        c.begin_conversion();
        c.advance();
        c.revert();
        assert_eq!(c.phase(), Phase::Reverted);
        assert_eq!(c.display(), "きょう");
        c.push_input("ha", "は", "");
        assert_eq!(c.phase(), Phase::Composing);
        assert_eq!(c.display(), "きょうはK1");
    }

    #[test]
    fn 空のときrevertしても空のまま() {
        let (mut c, _) = composer();
        c.revert();
        assert_eq!(c.phase(), Phase::Empty);
    }

    #[test]
    fn 候補選択は範囲内のみ成功し_テキスト指定でも選べる() {
        let (mut c, _) = composer();
        type_kyou(&mut c);
        c.begin_conversion();
        assert!(!c.select_candidate(9));
        assert!(c.select_candidate(2));
        assert_eq!(c.display(), "京");
        assert!(c.select_candidate_text("教"));
        assert_eq!(c.display(), "教");
        assert!(!c.select_candidate_text("存在しない"));
    }

    #[test]
    fn 未変換のままcommitすると生ひらがなを返しcancelする() {
        let (mut c, log) = composer();
        type_kyou(&mut c);
        assert_eq!(c.commit(true), "きょう");
        assert_eq!(*log.cancels.borrow(), 1);
        assert!(log.selects.borrow().is_empty());
        assert!(c.is_empty());
        assert_eq!(c.raw(), "");
    }

    #[test]
    fn 選択中の候補をcommitしその元indexで学習する() {
        let (mut c, log) = composer();
        type_kyou(&mut c);
        let generation = c.current_generation();
        c.begin_conversion();
        assert_eq!(c.commit(true), "今日");
        assert_eq!(*log.selects.borrow(), vec![(generation, 0)]);

        type_kyou(&mut c);
        c.begin_conversion();
        c.select_candidate(2);
        assert_eq!(c.commit(true), "京");
        assert_eq!(log.selects.borrow().last().unwrap().1, 1);
    }

    #[test]
    fn learnがfalseならcommitしても学習せずcancelする() {
        let (mut c, log) = composer();
        type_kyou(&mut c);
        c.begin_conversion();
        assert_eq!(c.commit(false), "今日");
        assert!(log.selects.borrow().is_empty());
        assert_eq!(*log.cancels.borrow(), 1);
    }

    #[test]
    fn raw候補をcommitしても学習せずcancelする() {
        let (mut c, log) = composer();
        type_kyou(&mut c);
        c.begin_conversion();
        c.advance();
        assert_eq!(c.commit(true), "kyou");
        assert!(log.selects.borrow().is_empty());
        assert_eq!(*log.cancels.borrow(), 1);
    }

    #[test]
    fn liveで変換前にcommitすると第1候補を学習して返す() {
        let (mut c, log) = composer();
        c.set_live(true);
        type_kyou(&mut c);
        assert_eq!(c.commit(true), "今日");
        assert_eq!(log.selects.borrow().last().unwrap().1, 0);
    }

    #[test]
    fn revert後のcommitは生ひらがなでcancelする() {
        let (mut c, log) = composer();
        c.set_live(true);
        type_kyou(&mut c);
        c.revert();
        assert_eq!(c.commit(true), "きょう");
        assert_eq!(*log.cancels.borrow(), 1);
    }

    #[test]
    fn 英単語は変換せずそのまま表示しcommitする() {
        let (mut c, log) = composer();
        c.push_literal("G");
        c.push_literal("it");
        assert_eq!(c.phase(), Phase::Literal);
        assert_eq!(c.display(), "Git");
        assert!(log.converts.borrow().is_empty());
        assert!(!c.begin_conversion());
        assert_eq!(c.commit(true), "Git");
        assert_eq!(*log.cancels.borrow(), 1);
        assert_eq!(c.phase(), Phase::Empty);
    }

    #[test]
    fn 英単語のbackspaceは1文字ずつ消え空で終わる() {
        let (mut c, _) = composer();
        c.push_literal("Hi");
        assert!(c.backspace());
        assert_eq!(c.display(), "H");
        assert_eq!(c.phase(), Phase::Literal);
        assert_eq!(c.raw(), "H");
        assert!(c.backspace());
        assert_eq!(c.phase(), Phase::Empty);
    }

    #[test]
    fn 非liveのリランキングは保留され最初の変換で反映される() {
        let (mut c, log) = composer();
        type_kyou(&mut c);
        let generation = c.current_generation();
        assert_eq!(c.apply_rerank(generation, 1), None);
        assert_eq!(c.display(), "きょう");
        assert_eq!(texts(&c), vec!["今日", "京", "教"]);
        c.begin_conversion();
        assert_eq!(c.display(), "京");
        assert_eq!(texts(&c), vec!["京", "kyou", "今日", "教"]);
        assert_eq!(c.commit(true), "京");
        assert_eq!(log.selects.borrow().last().unwrap().1, 1);
    }

    #[test]
    fn 保留中のリランキングは後の結果で上書きされる() {
        let (mut c, _) = composer();
        type_kyou(&mut c);
        let generation = c.current_generation();
        c.apply_rerank(generation, 1);
        c.apply_rerank(generation, 2);
        c.begin_conversion();
        assert_eq!(c.display(), "教");
    }

    #[test]
    fn 保留中のリランキングは入力が進むと破棄される() {
        let (mut c, _) = composer();
        type_kyou(&mut c);
        let generation = c.current_generation();
        c.apply_rerank(generation, 1);
        c.push_input("ha", "は", "");
        c.begin_conversion();
        assert_eq!(c.display(), "きょうはK1");
    }

    #[test]
    fn 変換直後で未操作ならリランキング結果を先頭へ反映しrawは直後に残す() {
        let (mut c, log) = composer();
        type_kyou(&mut c);
        let generation = c.current_generation();
        c.begin_conversion();
        assert_eq!(c.apply_rerank(generation, 2), Some("教".to_string()));
        assert_eq!(texts(&c), vec!["教", "kyou", "今日", "京"]);
        assert_eq!(
            c.phase(),
            Phase::Converting {
                selected: 0,
                window: false
            }
        );
        assert_eq!(c.commit(true), "教");
        assert_eq!(log.selects.borrow().last().unwrap().1, 2);
    }

    #[test]
    fn 送りの後はリランキングで並べ替えない() {
        let (mut c, _) = composer();
        type_kyou(&mut c);
        let generation = c.current_generation();
        c.begin_conversion();
        c.advance();
        assert_eq!(c.apply_rerank(generation, 2), None);
        assert_eq!(texts(&c), vec!["今日", "kyou", "京", "教"]);
    }

    #[test]
    fn 候補ウィンドウ表示中や候補選択後はリランキングで並べ替えない() {
        let (mut c, _) = composer();
        type_kyou(&mut c);
        let generation = c.current_generation();
        c.begin_conversion();
        assert!(c.select_candidate(2));
        assert_eq!(c.apply_rerank(generation, 1), None);
        assert_eq!(texts(&c), vec!["今日", "kyou", "京", "教"]);
        assert_eq!(c.display(), "京");
    }

    #[test]
    fn 変換直後でも旧世代や範囲外のリランキングは無効() {
        let (mut c, _) = composer();
        type_kyou(&mut c);
        let old = c.current_generation();
        c.push_input("ha", "は", "");
        c.begin_conversion();
        assert_eq!(c.apply_rerank(old, 1), None);

        let (mut c2, _) = composer();
        type_kyou(&mut c2);
        let gen2 = c2.current_generation();
        c2.begin_conversion();
        assert_eq!(c2.apply_rerank(gen2, 99), None);
        assert_eq!(c2.apply_rerank(gen2, 0), None);
        assert_eq!(texts(&c2), vec!["今日", "kyou", "京", "教"]);
    }

    #[test]
    fn liveのリランキングは即座に表示順を変えengine順位で学習する() {
        let (mut c, log) = composer();
        c.set_live(true);
        type_kyou(&mut c);
        let generation = c.current_generation();
        assert_eq!(c.apply_rerank(generation, 1), Some("京".to_string()));
        assert_eq!(c.display(), "京");
        assert_eq!(c.candidates()[0].engine_rank(), Some(EngineRank(1)));
        assert_eq!(c.candidates()[1].text, "今日");
        assert_eq!(c.commit(true), "京");
        assert_eq!(log.selects.borrow().last().unwrap().1, 1);
    }

    #[test]
    fn リランキングは旧世代_範囲外_先頭指定で無効() {
        let (mut c, _) = composer();
        type_kyou(&mut c);
        let old = c.current_generation();
        c.push_input("ha", "は", "");
        assert_eq!(c.apply_rerank(old, 1), None);

        let (mut c2, _) = composer();
        type_kyou(&mut c2);
        let gen2 = c2.current_generation();
        assert_eq!(c2.apply_rerank(gen2, 99), None);
        assert_eq!(c2.apply_rerank(gen2, 0), None);
        c2.begin_conversion();
        assert_eq!(c2.display(), "今日");
    }

    #[test]
    fn revert後のリランキングは無効() {
        let (mut c, _) = composer();
        c.set_live(true);
        type_kyou(&mut c);
        let generation = c.current_generation();
        c.revert();
        assert_eq!(c.apply_rerank(generation, 1), None);
    }

    use crate::keymap::{action_for, Action, Key};
    use crate::romaji_converter::RomajiConverter;

    fn type_word(c: &mut Composer<MockEngine>, conv: &mut RomajiConverter, word: &str) {
        for ch in word.chars() {
            let kana: String = conv.process_input(ch).unwrap_or_default();
            c.push_input(&ch.to_string(), &kana, conv.buffer());
        }
    }

    fn typed_and_flushed(word: &str) -> Composer<MockEngine> {
        let (mut c, _) = composer();
        let mut conv = RomajiConverter::new();
        type_word(&mut c, &mut conv, word);
        c.push_flush(conv.flush());
        c
    }

    #[test]
    fn 変換しきれない子音が残る語はenterでrawのまま確定する() {
        let mut c = typed_and_flushed("enter");
        assert_eq!(c.phase(), Phase::Dangling);
        assert_eq!(c.commit(true), "enter");
    }

    #[test]
    fn 変換しきれない子音が残る語はspaceでrawが第1候補になりかな候補が続く() {
        let mut c = typed_and_flushed("github");
        assert!(c.begin_conversion());
        assert_eq!(c.phase(), Phase::Converting { selected: 0, window: false });
        assert_eq!(c.display(), "github");
        assert_eq!(texts(&c)[0], "github");
        assert!(texts(&c).len() >= 2);
        assert_eq!(c.commit(true), "github");
        assert_eq!(*c.engine_cancels(), 1);
    }

    #[test]
    fn 末尾のnはかなのまま確定する() {
        let mut c = typed_and_flushed("kan");
        assert_eq!(c.phase(), Phase::Composing);
        assert_eq!(c.commit(true), "かん");
    }

    #[test]
    fn 全部かなになる語はrawが2番目の候補のまま() {
        let mut c = typed_and_flushed("kyou");
        assert_eq!(c.phase(), Phase::Composing);
        assert!(c.begin_conversion());
        assert_eq!(texts(&c)[1], "kyou");
        assert_eq!(c.display(), "今日");
    }

    #[test]
    fn ローマ字として解釈できない文字を含む語もrawが第1候補() {
        let mut c = typed_and_flushed("world");
        assert_eq!(c.phase(), Phase::Dangling);
        assert!(c.begin_conversion());
        assert_eq!(c.display(), "world");
    }

    #[test]
    fn 変換しきれない語に続く句読点はrawを確定してからasciiで入る() {
        let mut c = typed_and_flushed("enter");
        assert_eq!(
            action_for(c.phase(), Key::Punct),
            Action::CommitThenInsert
        );
        assert_eq!(format!("{}{}", c.commit(true), "."), "enter.");
    }

    #[test]
    fn 子音が残る語のbackspace後も残りがあれば英単語のまま() {
        let (mut c, _) = composer();
        let mut conv = RomajiConverter::new();
        type_word(&mut c, &mut conv, "entery");
        conv.handle_backspace();
        c.pop_pending_raw();
        c.push_flush(conv.flush());
        assert_eq!(c.phase(), Phase::Dangling);
        assert_eq!(c.commit(true), "enter");
    }

    fn type_symbol(c: &mut Composer<MockEngine>, ascii: &str) {
        let form = crate::symbols::default_form(ascii).unwrap_or(ascii);
        c.push_input(ascii, form, "");
    }

    #[test]
    fn 記号1つの組成は既定の和文表記を表示し_enterでそのまま確定する() {
        let (mut c, log) = composer();
        type_symbol(&mut c, "[");
        assert_eq!(c.phase(), Phase::Composing);
        assert_eq!(c.display(), "「");
        assert!(log.converts.borrow().is_empty());
        assert_eq!(c.commit(true), "「");
        assert!(log.selects.borrow().is_empty());
    }

    #[test]
    fn 記号だけの変換は異体の表から候補を出し_rawは足さず_学習もしない() {
        let (mut c, log) = composer();
        type_symbol(&mut c, "[");
        assert!(c.is_symbol());
        assert!(c.begin_conversion());
        assert_eq!(
            texts(&c),
            vec!["「", "『", "【", "〔", "［", "〈", "《", "(", "（", "｢"]
        );
        assert!(c.advance());
        assert_eq!(c.display(), "『");
        assert_eq!(c.commit(true), "『");
        assert!(log.selects.borrow().is_empty());
        assert_eq!(*c.engine_cancels(), 1);
    }

    #[test]
    fn 記号の候補は戻りと番号選択でも選べる() {
        let (mut c, _) = composer();
        type_symbol(&mut c, ".");
        c.begin_conversion();
        assert!(c.retreat());
        assert_eq!(c.display(), ".");
        assert!(c.select_candidate(1));
        assert_eq!(c.commit(true), "．");
    }

    #[test]
    fn 記号の後にローマ字を続けると辞書変換の組成になる() {
        let (mut c, log) = composer();
        let mut conv = RomajiConverter::new();
        type_symbol(&mut c, "[");
        type_word(&mut c, &mut conv, "kagi");
        type_symbol(&mut c, "]");
        assert_eq!(c.display_hiragana(), "「かぎ」");
        assert!(!c.is_symbol());
        assert_eq!(c.raw(), "[kagi]");
        assert_eq!(log.converts.borrow().last().map(|(_, h)| h.as_str()), Some("「かぎ」"));
        assert!(c.begin_conversion());
        assert_eq!(c.candidates()[0].text, "「かぎ」K1");
        assert_eq!(texts(&c)[1], "[kagi]");
    }

    #[test]
    fn 記号の直後の1文字だけのローマ字でも組成が続く() {
        let (mut c, _) = composer();
        let mut conv = RomajiConverter::new();
        type_symbol(&mut c, "(");
        type_word(&mut c, &mut conv, "a");
        c.push_flush(conv.flush());
        assert_eq!(c.display_hiragana(), "（あ");
        assert_eq!(c.commit(true), "（あ");
    }

    #[test]
    fn 記号を並べた組成は辞書変換に任せる() {
        let (mut c, _) = composer();
        type_symbol(&mut c, "[");
        type_symbol(&mut c, "]");
        assert!(!c.is_symbol());
        assert_eq!(c.display_hiragana(), "「」");
        assert!(c.begin_conversion());
        assert_eq!(texts(&c)[0], "「」K1");
    }

    #[test]
    fn かなを消して記号だけに戻ると異体の表に戻る() {
        let (mut c, _) = composer();
        type_symbol(&mut c, "[");
        c.push_input("a", "あ", "");
        assert!(!c.is_symbol());
        assert!(c.backspace());
        assert!(c.is_symbol());
        assert!(c.backspace());
        assert_eq!(c.phase(), Phase::Empty);
    }

    #[test]
    fn 記号は英単語入力の英字とは混ざらない() {
        let mut c = typed_and_flushed("world");
        assert_eq!(c.phase(), Phase::Dangling);
        assert!(!c.is_symbol());
        assert_eq!(c.commit(true), "world");
    }

    #[test]
    fn 字種変換は組成全体を指定の字種で表示し_確定しても学習しない() {
        let (mut c, log) = composer();
        type_kyou(&mut c);
        assert!(c.transliterate(Form::Katakana));
        assert_eq!(c.phase(), Phase::Transliterated(Form::Katakana));
        assert_eq!(c.display(), "キョウ");
        assert!(c.transliterate(Form::FullAlnum(translit::Case::Lower)));
        assert_eq!(c.display(), "ｋｙｏｕ");
        assert_eq!(c.commit(true), "ｋｙｏｕ");
        assert!(log.selects.borrow().is_empty());
        assert_eq!(c.phase(), Phase::Empty);
    }

    #[test]
    fn 同じ英数キーを続けると大文字小文字を巡る() {
        let (mut c, _) = composer();
        type_kyou(&mut c);
        let half = Form::HalfAlnum(translit::Case::Lower);
        assert!(c.transliterate(half));
        assert_eq!(c.display(), "kyou");
        assert!(c.transliterate(half));
        assert_eq!(c.display(), "KYOU");
        assert!(c.transliterate(half));
        assert_eq!(c.display(), "Kyou");
        assert!(c.transliterate(half));
        assert_eq!(c.display(), "kyou");
    }

    #[test]
    fn 末尾が変換しきれない組成のかな形は子音をそのまま残し_英数形は打鍵から作る() {
        let (mut c, _) = composer();
        c.push_input("e", "え", "");
        c.push_input("n", "", "n");
        c.push_input("n", "ん", "");
        c.push_input("te", "て", "");
        c.push_input("r", "", "r");
        c.push_flush(Flush::Dangling("r".to_string()));
        assert_eq!(c.phase(), Phase::Dangling);
        c.transliterate(Form::Katakana);
        assert_eq!(c.display(), "エンテr");
        c.transliterate(Form::HalfKatakana);
        assert_eq!(c.display(), "ｴﾝﾃr");
        c.transliterate(Form::HalfAlnum(translit::Case::Lower));
        assert_eq!(c.display(), "ennter");
    }

    #[test]
    fn 字種変換のあとスペースで元のひらがなから通常変換に入る() {
        let (mut c, _) = composer();
        type_kyou(&mut c);
        c.transliterate(Form::Katakana);
        assert!(c.begin_conversion());
        assert!(matches!(c.phase(), Phase::Converting { selected: 0, .. }));
        assert_eq!(c.display(), "今日");
    }

    #[test]
    fn 字種変換のあとescでひらがなへ戻りbackspaceは1文字削る() {
        let (mut c, _) = composer();
        type_kyou(&mut c);
        c.transliterate(Form::Katakana);
        c.revert();
        assert_eq!(c.phase(), Phase::Reverted);
        assert_eq!(c.display(), "きょう");
        c.transliterate(Form::HalfKatakana);
        assert!(c.backspace());
        assert_eq!(c.phase(), Phase::Composing);
        assert_eq!(c.display(), "きょ");
    }

    #[test]
    fn discardは組成を何も確定せず捨てて学習もしない() {
        let (mut c, log) = composer();
        type_kyou(&mut c);
        c.begin_conversion();
        c.discard();
        assert_eq!(c.phase(), Phase::Empty);
        assert!(c.is_empty());
        assert_eq!(c.raw(), "");
        assert!(c.candidates().is_empty());
        assert!(log.selects.borrow().is_empty());
    }

    #[test]
    fn かなの途中の大文字は英単語として加わりmixedになる() {
        let (mut c, _) = composer();
        type_kyou(&mut c);
        c.push_literal("T");
        assert_eq!(c.phase(), Phase::Mixed);
        c.push_literal("okyo");
        assert_eq!(c.display(), "きょうTokyo");
        assert_eq!(c.raw(), "kyouTokyo");
    }

    #[test]
    fn mixedの英単語部分をbackspaceで消しきると通常の組成に戻る() {
        let (mut c, _) = composer();
        type_kyou(&mut c);
        c.push_literal("To");
        assert!(c.backspace());
        assert_eq!(c.display(), "きょうT");
        assert_eq!(c.phase(), Phase::Mixed);
        assert!(c.backspace());
        assert_eq!(c.display(), "きょう");
        assert_eq!(c.phase(), Phase::Composing);
        assert_eq!(texts(&c), vec!["今日", "京", "教"]);
    }

    #[test]
    fn mixedの変換はかな混じりが第1候補でrawがその直後に並ぶ() {
        let (mut c, log) = composer();
        c.push_input("i", "い", "");
        c.push_literal("Phone");
        assert!(c.begin_conversion());
        assert_eq!(texts(&c), vec!["いPhone", "iPhone"]);
        assert_eq!(c.commit(true), "いPhone");
        assert!(log.selects.borrow().is_empty());
    }

    #[test]
    fn 大文字を含む打鍵の英数形は打鍵のまま始まり小文字_大文字_先頭大文字と巡る() {
        let (mut c, _) = composer();
        c.push_literal("Tokyo");
        let half = Form::HalfAlnum(translit::Case::Lower);
        for want in ["Tokyo", "tokyo", "TOKYO", "Tokyo", "tokyo"] {
            assert!(c.transliterate(half));
            assert_eq!(c.display(), want);
        }
        let full = Form::FullAlnum(translit::Case::Lower);
        c.revert();
        assert!(c.transliterate(full));
        assert_eq!(c.display(), "Ｔｏｋｙｏ");
    }

    #[test]
    fn 空の組成は字種変換しない() {
        let (mut c, _) = composer();
        assert!(!c.transliterate(Form::Katakana));
        assert_eq!(c.phase(), Phase::Empty);
    }

    fn loanwords(katakana: &str) -> Vec<String> {
        match katakana {
            "デスクトップ" => vec!["desktop".to_string()],
            "エンター" => vec!["enter".to_string()],
            "ライト" => vec!["light".to_string(), "right".to_string()],
            _ => Vec::new(),
        }
    }

    fn loan_composer(reading: &str, candidates: &[&str]) -> Composer<MockEngine> {
        let engine = MockEngine {
            log: Log::default(),
            table: vec![(
                reading.to_string(),
                candidates.iter().map(|s| s.to_string()).collect(),
            )],
        };
        Composer::new(engine).with_english(loanwords)
    }

    #[test]
    fn カタカナ語は英単語が第1候補とrawの間に入る() {
        let mut c = loan_composer("ですくとっぷ", &["デスクトップ", "デス区トップ"]);
        c.push_input("desukutoppu", "ですくとっぷ", "");
        c.begin_conversion();
        assert_eq!(
            texts(&c),
            vec!["デスクトップ", "desktop", "desukutoppu", "デス区トップ"]
        );
        assert_eq!(c.candidates()[1].engine_rank(), None);
    }

    #[test]
    fn 英単語候補を選んで確定するとその文字列が出て学習しない() {
        let mut c = loan_composer("ですくとっぷ", &["デスクトップ"]);
        c.push_input("desukutoppu", "ですくとっぷ", "");
        c.begin_conversion();
        assert!(c.advance());
        assert_eq!(c.display(), "desktop");
        assert_eq!(c.commit(true), "desktop");
        assert!(c.engine.log.selects.borrow().is_empty());
    }

    #[test]
    fn 小書きのかなを含む語もrawとカタカナ語の候補を持つ() {
        let engine = MockEngine {
            log: Log::default(),
            table: vec![("うぃんど".to_string(), vec!["ウィンド".to_string()])],
        };
        let mut c = Composer::new(engine).with_english(|k| {
            if k == "ウィンド" { vec!["wind".to_string()] } else { Vec::new() }
        });
        let mut conv = RomajiConverter::new();
        type_word(&mut c, &mut conv, "uxinndo");
        c.push_flush(conv.flush());
        c.begin_conversion();
        assert_eq!(texts(&c), vec!["ウィンド", "wind", "uxinndo"]);
    }

    #[test]
    fn 複数の英単語は表の順に並ぶ() {
        let mut c = loan_composer("らいと", &["ライト"]);
        c.push_input("raito", "らいと", "");
        c.begin_conversion();
        assert_eq!(texts(&c), vec!["ライト", "light", "right", "raito"]);
    }

    #[test]
    fn 表に無い語は候補が増えない() {
        let (mut c, _) = composer();
        type_kyou(&mut c);
        c.begin_conversion();
        assert_eq!(texts(&c), vec!["今日", "kyou", "京", "教"]);
    }

    #[test]
    fn 末尾の子音が残るDanglingでは英単語を引かない() {
        let mut c = loan_composer("ですくとっぷ", &["デスクトップ"]);
        c.push_input("desukutoppu", "ですくとっぷ", "");
        c.push_flush(Flush::Dangling("r".to_string()));
        assert_eq!(c.phase(), Phase::Dangling);
        c.begin_conversion();
        assert!(!texts(&c).contains(&"desktop"));
    }

    #[test]
    fn 文の一部だけがカタカナ語でも引かない() {
        let mut c = loan_composer("ですくとっぷをみる", &["デスクトップを見る"]);
        c.push_input("desukutoppuwomiru", "ですくとっぷをみる", "");
        c.begin_conversion();
        assert!(!texts(&c).contains(&"desktop"));
    }

    #[test]
    fn 英単語がrawと同じなら一度だけ入る() {
        let mut c = loan_composer("えんたー", &["エンター"]);
        c.push_input("enter", "えんたー", "");
        c.begin_conversion();
        assert_eq!(texts(&c), vec!["エンター", "enter"]);
    }

    #[test]
    fn リランキング後も英単語とrawは第1候補の直後に並ぶ() {
        let mut c = loan_composer("ですくとっぷ", &["デスクトップ", "デス区トップ"]);
        c.push_input("desukutoppu", "ですくとっぷ", "");
        c.begin_conversion();
        assert_eq!(c.apply_rerank(c.current_generation(), 1).as_deref(), Some("デス区トップ"));
        assert_eq!(
            texts(&c),
            vec!["デス区トップ", "desktop", "desukutoppu", "デスクトップ"]
        );
    }

    fn emojis(reading: &str) -> Vec<String> {
        match reading {
            "ねこ" => ["😺", "😸", "😹", "😻", "😼", "🐱", "🐈"].map(String::from).to_vec(),
            "かんぱい" => vec!["🍻".to_string(), "X1".to_string()],
            "いぬ" => vec!["🐶".to_string()],
            _ => Vec::new(),
        }
    }

    fn emoji_composer(reading: &str, candidates: &[&str]) -> Composer<MockEngine> {
        let engine = MockEngine {
            log: Log::default(),
            table: vec![(
                reading.to_string(),
                candidates.iter().map(|s| s.to_string()).collect(),
            )],
        };
        Composer::new(engine).with_emoji(emojis)
    }

    #[test]
    fn 絵文字は上位3件のエンジン候補とrawの後ろに入り最大5件() {
        let mut c = emoji_composer("ねこ", &["猫", "ネコ", "根子", "寝こ", "値こ"]);
        c.push_input("neko", "ねこ", "");
        c.begin_conversion();
        assert_eq!(
            texts(&c),
            vec!["猫", "neko", "ネコ", "根子", "😺", "😸", "😹", "😻", "😼", "寝こ", "値こ"]
        );
        assert_eq!(c.candidates()[4].engine_rank(), None);
    }

    #[test]
    fn 絵文字は3番目より前には入らない() {
        let mut c = emoji_composer("いぬ", &["犬"]);
        c.push_input("inu", "いぬ", "");
        c.begin_conversion();
        assert_eq!(texts(&c), vec!["犬", "inu", "🐶"]);
        let mut c = emoji_composer("いぬ", &[]);
        c.push_input("inu", "いぬ", "");
        c.begin_conversion();
        assert_eq!(texts(&c), vec!["いぬ", "inu", "イヌ", "🐶"]);
    }

    #[test]
    fn 絵文字を選んで確定するとその文字列が出て学習しない() {
        let mut c = emoji_composer("かんぱい", &["乾杯", "完敗", "缶杯"]);
        c.push_input("kanpai", "かんぱい", "");
        c.begin_conversion();
        let index = texts(&c).iter().position(|t| *t == "🍻").unwrap();
        assert!(c.select_candidate(index));
        assert_eq!(c.commit(true), "🍻");
        assert!(c.engine.log.selects.borrow().is_empty());
    }

    #[test]
    fn 読みが表に無ければ候補は変わらない() {
        let (mut c, _) = composer();
        type_kyou(&mut c);
        c.begin_conversion();
        assert_eq!(texts(&c), vec!["今日", "kyou", "京", "教"]);
    }

    #[test]
    fn 末尾の子音が残るDanglingでは絵文字を引かない() {
        let mut c = emoji_composer("ねこ", &["猫"]);
        c.push_input("neko", "ねこ", "");
        c.push_flush(Flush::Dangling("r".to_string()));
        c.begin_conversion();
        assert!(!texts(&c).contains(&"😺"));
    }

    #[test]
    fn かな混じりのMixedでは絵文字を引かない() {
        let mut c = emoji_composer("ねこ", &["猫"]);
        c.push_input("neko", "ねこ", "");
        c.push_literal("A");
        c.begin_conversion();
        assert!(!texts(&c).contains(&"😺"));
    }

    #[test]
    fn 記号の組成では絵文字を引かない() {
        let engine = MockEngine { log: Log::default(), table: vec![] };
        let mut c = Composer::new(engine).with_emoji(|_| vec!["😺".to_string()]);
        c.push_input("!", "！", "");
        c.begin_conversion();
        assert!(!texts(&c).contains(&"😺"));
    }

    #[test]
    fn 既にある候補と同じ絵文字は入れない() {
        let mut c = emoji_composer("かんぱい", &["乾杯", "🍻"]);
        c.push_input("kanpai", "かんぱい", "");
        c.begin_conversion();
        assert_eq!(texts(&c).iter().filter(|t| **t == "🍻").count(), 1);
        assert!(texts(&c).contains(&"X1"));
    }

    #[test]
    fn リランキング後も絵文字はエンジン候補の後ろに並ぶ() {
        let mut c = emoji_composer("いぬ", &["犬", "戌", "乾", "伊奴"]);
        c.push_input("inu", "いぬ", "");
        c.begin_conversion();
        assert_eq!(texts(&c), vec!["犬", "inu", "戌", "乾", "🐶", "伊奴"]);
        assert_eq!(c.apply_rerank(c.current_generation(), 1).as_deref(), Some("戌"));
        assert_eq!(texts(&c), vec!["戌", "inu", "犬", "乾", "🐶", "伊奴"]);
    }
}
