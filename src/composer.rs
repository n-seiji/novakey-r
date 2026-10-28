#![forbid(unsafe_code)]

// composer.rs - 未確定文の状態機械: 入力単位(ローマ字+かな) + 同期変換候補 + フェーズ。
// 候補は {text, source} で持ち、リランキング後も学習には Source::Engine の元の順位を渡す。
// ローマ字そのまま(raw)など engine 以外の候補は順位を持たず、確定しても学習しない。

use std::borrow::Cow;

use crate::deps::Lexicon;
use crate::english_split;
use crate::kkc::live_backspace;
use crate::romaji_converter::{Flush, RomajiConverter};
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

/// 確定の結果。学習は「何を学習すべきか」だけを持ち、実行は Session が行う。
/// `engine` が None なら Session は engine を取り消す (保留中の学習状態を残さない)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Committed {
    pub text: String,
    /// engine に学習させる (確定時の世代, 元の順位)。
    pub engine: Option<(u64, EngineRank)>,
    /// 英語として確定した語。
    pub english: Vec<String>,
}

pub trait CandidateEngine {
    fn convert(&mut self, revision: u64, hiragana: &str) -> Vec<String>;
    fn select(&mut self, revision: u64, engine_index: usize) -> bool;
    fn cancel(&mut self);
    /// 変換の文脈 (アプリで直前に確定した文字列。空なら文脈なし)。キーごとに、変換する前に渡される。
    fn set_context(&mut self, _context: &str) {}
    /// ひらがなの読みがそのまま辞書の見出しにあるか (made → まで)。英単語として切り分ける前の衝突確認に使う。
    fn has_reading(&self, _hiragana: &str) -> bool {
        false
    }
    /// `text` の方が `over` より、いまの文脈で文として自然か。審判の差が余裕を超えたときだけ true。審判が使えない engine は false。
    fn prefers_text(&mut self, _text: &str, _over: &str) -> bool {
        false
    }
    /// 英字 `raw` より読み `reading` (engine の第 1 候補) の方が、はっきり文として自然か。審判が使えない engine は false。
    fn rejects_english(&mut self, _raw: &str, _reading: &str) -> bool {
        false
    }
    /// 辞書を開けているか。開けていない間は見出しの判定ができないので、英単語の自動判定をしない。
    fn has_dictionary(&self) -> bool {
        true
    }
    /// 直近の `convert` の候補 `index` の語列 ((表記, カタカナの読み))。読むだけで、学習用の状態は取り出さない。
    /// 語の情報を持たない engine や、世代の合わない呼び出しは None。
    fn words(&self, _revision: u64, _index: usize) -> Option<Vec<(String, String)>> {
        None
    }
    /// カタカナの読みで、表記が漢字 1 字の見出しが辞書にあるか。live 変換の Backspace が読みを切り分けるのに使う。
    fn has_kanji_reading(&self, _katakana: &str, _kanji: char) -> bool {
        false
    }
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

/// 入力中の打鍵の解釈。Empty は未確定文が無い状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Typing {
    Empty,
    Kana,
    /// 英語を打っていると判定した間。打鍵ごとに判定し直し、engine の変換は止める。
    English,
    Literal,
    Mixed,
}

/// flush 後の組成の扱い。
enum Settle {
    /// 英単語とかなの混在 (Mixed) に組み替える。
    Mixed(english_split::Split),
    Dangling,
    Unchanged,
}

/// 組成全体が英単語のときの判定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// 読みを使う。
    Kana,
    /// かなの読みが辞書の見出しでない語。審判が読みを選ばない限り英語。
    Word,
    /// かなの読みが辞書の見出しでもある語を、審判が英語と決めた。学習しない (見出しの除外を `knows_english` が素通りさせるため)。
    Judged,
}

/// 読みが変わらない間 (= 同じ generation の間) 生き続ける、飾りなしの候補列。
#[derive(Debug)]
struct ReadingCands {
    generation: u64,
    list: Vec<Candidate>,
}

/// 候補を選んでいる間の状態。list は空でない。
#[derive(Debug)]
struct Selection {
    list: Vec<Candidate>,
    cursor: usize,
    /// 候補ウィンドウを出している間 true。
    window: bool,
}

/// 組成の段階。局面ごとにしか意味のないデータは variant の中に置く。打鍵の解釈 (Typing) は段階をまたいで Composer が持つ。
#[derive(Debug)]
enum Stage {
    Editing,
    /// 変換を取り消して読みを表示している。Typing::Kana のときだけ。
    /// kept は、取り消す直前に見えていた (飾り付きの) 候補列。再変換はこの列から続ける。
    Reverted { kept: Option<Vec<Candidate>> },
    Selecting(Selection),
    /// 字種変換の結果を表示している。
    Transliterated {
        form: Form,
        text: String,
        kept: Option<Vec<Candidate>>,
    },
}

/// live 変換の Backspace で残した、見えているとおりに固定する読みの先頭部分。
/// 読み (ひらがな) の先頭から順に並ぶ (表記, カタカナの読み)。固定した読みの長さは語の読みの合計。
#[derive(Debug, Clone)]
struct Pin {
    words: Vec<(String, String)>,
}

impl Pin {
    fn reading_len(&self) -> usize {
        self.words.iter().map(|(_, ruby)| ruby.chars().count()).sum()
    }

    fn surface(&self) -> String {
        self.words.iter().map(|(surface, _)| surface.as_str()).collect()
    }

    fn ruby(&self) -> String {
        self.words.iter().map(|(_, ruby)| ruby.as_str()).collect()
    }
}

/// 組成の 1 区切り。
#[derive(Debug, Clone)]
enum Segment {
    /// 1 回の入力で生まれた (打鍵, かな) の組。部分削除後は keys をかなで置き換える。
    Kana { keys: String, kana: String },
    /// 大文字から始めた英単語など、かな変換せずそのまま入れた部分。
    Literal(String),
}

impl Segment {
    fn keys(&self) -> &str {
        match self {
            Segment::Kana { keys, .. } => keys,
            Segment::Literal(text) => text,
        }
    }

    fn reading(&self) -> &str {
        match self {
            Segment::Kana { kana, .. } => kana,
            Segment::Literal(text) => text,
        }
    }

    fn is_literal(&self) -> bool {
        matches!(self, Segment::Literal(_))
    }
}

/// 1 回の変換で入れる絵文字の上限
const EMOJI_LIMIT: usize = 5;
/// 絵文字より前に並べるエンジン候補の数
const EMOJI_AFTER_ENGINE: usize = 3;
/// live 変換は、読みがこの文字数 (かな) を超えてから表示を変換する。これ以下はひらがなのまま (Space の変換は別)。
const LIVE_MIN_KANA: usize = 2;

pub struct Composer<E: CandidateEngine, L: Lexicon> {
    engine: E,
    segments: Vec<Segment>,
    /// 未確定のローマ字 (まだかなにならない打鍵) の唯一の持ち主。
    romaji: RomajiConverter,
    hiragana: String,
    typing: Typing,
    /// Typing::English が、打鍵中の表示で英字として見えていたか。flush が初めて英語と見なした組成は false。
    english_shown: bool,
    /// 英語分割で英単語にまとめる前の区切りと未確定の打鍵。取り消しで分割前の打鍵中に戻すために持つ。
    split_origin: Option<(Vec<Segment>, String)>,
    /// live 変換で英語分割を表示している間の、(英単語, かな部分, 英単語 + 変換したかなの候補列)。
    /// 見えている分割と、Enter で確定される分割 (未確定のローマ字を確定かなとした分割) が違えば両方持つ。
    /// 打鍵が英単語とかな部分を変えなければ変換し直さない。`reading` とは別に持つ。
    /// 分割を表示している間は読み全体を変換しない (engine の直前の変換と `candidates()` を食い違わせない)。
    live_split: Vec<(String, String, Vec<Candidate>)>,
    reading: ReadingCands,
    stage: Stage,
    live: bool,
    /// 固定中の先頭部分。あるあいだ `reading` の候補は残りの読み (固定した分のあと) の変換結果。
    pin: Option<Pin>,
    /// 組成全体 (raw) が英単語のときの判定。raw が違えば読まない。
    word_verdict: Option<(String, Verdict)>,
    lexicon: L,
}

impl<E: CandidateEngine, L: Lexicon> Composer<E, L> {
    pub fn new(engine: E, lexicon: L) -> Self {
        Self {
            engine,
            segments: Vec::new(),
            romaji: RomajiConverter::new(),
            hiragana: String::new(),
            typing: Typing::Empty,
            english_shown: false,
            split_origin: None,
            live_split: Vec::new(),
            reading: ReadingCands { generation: 0, list: Vec::new() },
            stage: Stage::Editing,
            live: false,
            pin: None,
            word_verdict: None,
            lexicon,
        }
    }

    pub fn set_live(&mut self, live: bool) {
        if self.live != live {
            self.live = live;
            self.unpin();
            self.sync_live_split();
        }
    }

    pub fn is_live(&self) -> bool {
        self.live
    }

    pub fn phase(&self) -> Phase {
        match &self.stage {
            Stage::Editing => match self.typing {
                Typing::Empty => Phase::Empty,
                Typing::Kana => Phase::Composing,
                Typing::English => Phase::Dangling,
                Typing::Literal => Phase::Literal,
                Typing::Mixed => Phase::Mixed,
            },
            Stage::Reverted { .. } => Phase::Reverted,
            Stage::Selecting(s) => Phase::Converting { selected: s.cursor, window: s.window },
            Stage::Transliterated { form, .. } => Phase::Transliterated(*form),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.hiragana.is_empty()
    }

    /// いま見えている候補列。選択中と、選択中だった列を持ち越している取り消し・字種変換の間は飾り付き。
    pub fn candidates(&self) -> &[Candidate] {
        match &self.stage {
            Stage::Selecting(s) => &s.list,
            Stage::Reverted { kept: Some(list) } | Stage::Transliterated { kept: Some(list), .. } => list,
            // 固定中の reading.list は残りの読みだけの候補で、見えている文字列 (固定 + 残り) と食い違う。
            _ if self.pin.is_some() => &[],
            // 英語分割を表示している間は、読み全体の候補を見せない (見えている文字列と食い違う)。
            _ if !self.live_split.is_empty() => &[],
            _ => &self.reading.list,
        }
    }

    /// 打鍵したままの ASCII 文字列(かなに変換済みの分と未変換の分)。
    pub fn raw(&self) -> String {
        self.segments
            .iter()
            .map(Segment::keys)
            .chain(std::iter::once(self.romaji.buffer()))
            .collect()
    }

    pub fn display(&self) -> Cow<'_, str> {
        match &self.stage {
            Stage::Selecting(s) => Cow::Borrowed(&s.list[s.cursor].text),
            Stage::Editing if self.typing == Typing::Kana && self.live_converts() => match &self.pin {
                None => Cow::Borrowed(self.reading.list.first().map_or(self.hiragana.as_str(), |c| c.text.as_str())),
                Some(pin) => {
                    let converted = self.reading.list.first().filter(|c| c.engine_rank().is_some());
                    Cow::Owned(pin.surface() + converted.map_or(self.rest(), |c| c.text.as_str()))
                }
            },
            Stage::Editing if self.typing == Typing::Mixed && self.live => {
                let converted = self.reading.list.first().filter(|c| c.engine_rank().is_some());
                Cow::Borrowed(converted.map_or(self.hiragana.as_str(), |c| c.text.as_str()))
            }
            Stage::Transliterated { text, .. } => Cow::Borrowed(text),
            _ => Cow::Borrowed(&self.hiragana),
        }
    }

    /// 下線付きで見せる文字列。表示の決定はここだけ。
    pub fn preedit(&self) -> String {
        if let Some(split) = self.shown_split() {
            return match self.live_split_text(&split) {
                Some(text) => text.to_string() + &split.pending,
                None => split.display(),
            };
        }
        if (self.typing == Typing::English && self.is_kana_editing()) || self.is_typing_english() {
            return self.raw();
        }
        format!("{}{}", self.display(), self.romaji.buffer())
    }

    /// Enter で確定される文字列 (状態は変えない)。未確定ローマ字は確定済みとして扱わない。
    /// かな入力中は、確定の前に行う flush が局面を英語に変える分 (merge, artifactni) も見込む。
    pub fn commit_preview(&self) -> String {
        match self.settle(self.is_typing_english()) {
            Settle::Mixed(split) => match self.live_split_text(&split) {
                Some(text) => text.to_string(),
                None => format!("{}{}", split.head, split.tail_kana()),
            },
            Settle::Dangling => self.raw(),
            Settle::Unchanged if self.phase() == Phase::Dangling => self.raw(),
            Settle::Unchanged => self.display().into_owned(),
        }
    }

    /// 組成全体を form の字種で表示する。直前も同じ種類の字種変換なら、英数形は大文字小文字を巡らせる。
    pub fn transliterate(&mut self, requested: Form) -> bool {
        if self.hiragana.is_empty() {
            return false;
        }
        self.unpin();
        let current = match &self.stage {
            Stage::Transliterated { form, .. } => Some(*form),
            _ => None,
        };
        let requested = translit::as_typed_first(requested, &self.raw());
        let form = translit::next_form(current, requested);
        let rendered = translit::render(form, &self.hiragana, &self.raw());
        if let Stage::Transliterated { form: f, text, .. } = &mut self.stage {
            *f = form;
            *text = rendered;
            return true;
        }
        let kept = match std::mem::replace(&mut self.stage, Stage::Editing) {
            Stage::Editing => None,
            Stage::Reverted { kept } => kept,
            Stage::Selecting(s) => {
                // 設計書は「Transliterated → Space は typing のまま」だが、候補選択中からの字種変換だけは Dangling を Kana に落とす。
                // 候補選択中は読みの変換結果を見ていて、取り消し (revert) と同じく読みに戻るため。Editing からの字種変換は Dangling のまま。
                self.leave_english();
                Some(s.list)
            }
            Stage::Transliterated { .. } => unreachable!("handled above"),
        };
        self.stage = Stage::Transliterated { form, text: rendered, kept };
        true
    }

    fn sync_hiragana(&mut self) {
        self.hiragana = self.segments.iter().map(Segment::reading).collect();
    }

    fn reset_candidates(&mut self) {
        self.reading.generation += 1;
        self.reading.list.clear();
        self.live_split.clear();
        if let Stage::Reverted { kept } | Stage::Transliterated { kept, .. } = &mut self.stage {
            *kept = None;
        }
    }

    fn refresh(&mut self) {
        self.reset_candidates();
        self.sync_hiragana();
        self.stage = Stage::Editing;
        if self.hiragana.is_empty() {
            self.pin = None;
            self.typing = Typing::Empty;
            return;
        }
        self.typing = Typing::Kana;
        self.word_verdict = None;
        if self.is_typing_english() {
            self.enter_english();
            return;
        }
        if let Some(forms) = self.symbol_variants() {
            self.pin = None;
            self.reading.list = forms
                .iter()
                .map(|form| Candidate {
                    text: (*form).to_string(),
                    source: Source::Symbol,
                })
                .collect();
            return;
        }
        if !self.live_converts() || !self.pin_matches_reading() {
            self.pin = None;
        }
        self.sync_live_split();
        if self.live_split.is_empty() {
            self.reading.list = self.convert_reading();
            self.enter_english_if_word();
        }
    }

    /// 組成全体が英単語なら判定し、英語になったら英語を打っている局面に入る (読みの候補は捨てる)。
    fn enter_english_if_word(&mut self) {
        self.judge_word();
        if self.is_typing_english() {
            self.reset_candidates();
            self.enter_english();
        }
    }

    /// 英語を打っている局面に入る。読みの候補は呼び出し側で捨て済み。
    fn enter_english(&mut self) {
        self.pin = None;
        self.typing = Typing::English;
        self.english_shown = true;
        self.sync_live_split();
    }

    /// 固定した先頭部分が、いまの読みの先頭と合っているか。固定が無ければ true。
    fn pin_matches_reading(&self) -> bool {
        self.pin.as_ref().is_none_or(|pin| {
            let head: String = self.hiragana.chars().take(pin.reading_len()).collect();
            head.chars().count() == pin.reading_len() && translit::to_katakana(&head) == pin.ruby()
        })
    }

    /// 固定した先頭部分のあとの読み。固定が無ければ読み全体。
    fn rest(&self) -> &str {
        let skip = self.pin.as_ref().map_or(0, Pin::reading_len);
        let at = self.hiragana.char_indices().nth(skip).map_or(self.hiragana.len(), |(i, _)| i);
        &self.hiragana[at..]
    }

    /// 固定を外して読み全体を変換し直す。読みはそのまま。
    fn unpin(&mut self) {
        if self.pin.take().is_none() {
            return;
        }
        self.reset_candidates();
        if matches!(self.stage, Stage::Editing) && self.typing == Typing::Kana {
            self.reading.list = self.convert_reading();
            self.enter_english_if_word();
        }
    }

    fn convert_reading(&mut self) -> Vec<Candidate> {
        let mut list: Vec<Candidate> = Vec::new();
        let rest = self.rest().to_string();
        for (engine_index, text) in self.engine.convert(self.reading.generation, &rest).into_iter().enumerate() {
            if !list.iter().any(|c| c.text == text) {
                list.push(Candidate { text, source: Source::Engine(EngineRank(engine_index)) });
            }
        }
        list
    }

    /// 組成全体が記号 1 つだけのときの異体候補。かなと混ざれば None(辞書変換に任せる)。
    fn symbol_variants(&self) -> Option<&'static [&'static str]> {
        match self.segments.as_slice() {
            [_] => symbols::variants(&self.hiragana),
            _ => None,
        }
    }

    /// 組成全体が記号 1 つだけか。学習の対象にしない。
    pub fn is_symbol(&self) -> bool {
        self.symbol_variants().is_some()
    }

    /// 未確定のローマ字を確定かなとして取り込む。Dangling なら局面を Dangling にする。
    pub fn flush_romaji(&mut self) {
        let english = self.is_typing_english();
        let keys = self.romaji.buffer().to_string();
        let flush = self.romaji.flush();
        self.absorb_flush(english, keys, flush);
    }

    /// english は、未確定のローマ字を残した状態で英語と見なせていたか。
    fn absorb_flush(&mut self, english: bool, keys: String, flush: Flush) {
        match flush {
            Flush::Empty => {}
            Flush::Kana(kana) | Flush::Dangling(kana) => self.push_kana(keys, &kana),
        }
        match self.settle(english) {
            Settle::Mixed(split) => self.fix_english_split(split),
            Settle::Dangling => {
                self.pin = None;
                self.typing = Typing::English;
            }
            Settle::Unchanged => {}
        }
        if self.typing == Typing::English {
            self.english_shown = english;
        }
    }

    /// ローマ字を確定し終えた組成が、英語として扱い直されるか。
    fn settle(&self, english: bool) -> Settle {
        if let Some(split) = self.english_split(true) {
            return Settle::Mixed(split);
        }
        if self.is_kana_editing() && (english || self.has_ascii_kana()) {
            return Settle::Dangling;
        }
        Settle::Unchanged
    }

    fn is_kana_editing(&self) -> bool {
        matches!(self.stage, Stage::Editing) && matches!(self.typing, Typing::Kana | Typing::English)
    }

    /// 打鍵のあと、英語を打っているかの判定を状態に反映する。
    /// 英語になった時点で読みの候補は捨てる。
    fn track_english(&mut self) {
        if !self.is_kana_editing() {
            return;
        }
        let raw = self.raw();
        if raw.bytes().all(|b| b.is_ascii_lowercase()) && self.is_whole_word(&raw) {
            // 打鍵を消して全体が英単語になった (mades → made)。変換と判定は refresh が 1 度で行う。
            self.refresh();
            return;
        }
        self.word_verdict = None;
        match (self.is_typing_english(), self.typing == Typing::English) {
            (true, false) => {
                self.pin = None;
                self.reset_candidates();
                self.typing = Typing::English;
                self.english_shown = true;
            }
            (false, true) => {
                self.refresh();
                return;
            }
            _ => {}
        }
        self.sync_live_split();
    }

    /// live 変換で英語分割を表示している間、英単語 + かな部分の変換を保つ。
    /// 英単語とかな部分が変わらない打鍵 (未確定のローマ字だけが動く) では変換し直さない。
    fn sync_live_split(&mut self) {
        let shown = if self.live { self.shown_split() } else { None };
        let settled = shown.as_ref().and_then(|_| self.english_split(true));
        let mut wanted: Vec<(String, String)> = Vec::new();
        for split in shown.into_iter().chain(settled) {
            let key = (split.head.clone(), split.tail_kana());
            if !wanted.contains(&key) {
                wanted.push(key);
            }
        }
        let unchanged = wanted.len() == self.live_split.len()
            && wanted.iter().zip(&self.live_split).all(|((head, tail), (h, t, _))| head == h && tail == t);
        if unchanged {
            return;
        }
        let was_split = !self.live_split.is_empty();
        if !wanted.is_empty() {
            self.pin = None;
        }
        self.live_split = wanted
            .into_iter()
            .map(|(head, tail)| {
                let list = self.convert_mixed(&head, &tail);
                (head, tail, list)
            })
            .collect();
        // 分割が消えたら、飛ばしていた読み全体の変換を戻す。
        if was_split && self.live_split.is_empty() && self.typing == Typing::Kana && matches!(self.stage, Stage::Editing) {
            self.reading.list = self.convert_reading();
        }
    }

    /// 下線付きで見せている英語分割。未確定のローマ字が無ければ、Enter で確定される分割と同じ。
    fn shown_split(&self) -> Option<english_split::Split> {
        self.english_split(self.live && self.romaji.buffer().is_empty())
    }

    /// いま見えている英語分割の、変換済みの表示 (英単語 + 変換したかな)。変換が無い・分割が違うなら None。
    fn live_split_text(&self, split: &english_split::Split) -> Option<&str> {
        if !self.live {
            return None;
        }
        let tail = split.tail_kana();
        let (_, _, list) = self.live_split.iter().find(|(head, kana, _)| *head == split.head && *kana == tail)?;
        list.first().filter(|c| c.engine_rank().is_some()).map(|c| c.text.as_str())
    }

    /// 英単語 head のあとに、かな tail をエンジンで変換したものを付けた候補列。
    fn convert_mixed(&mut self, head: &str, tail: &str) -> Vec<Candidate> {
        let mut list: Vec<Candidate> = Vec::new();
        for (engine_index, text) in self.engine.convert(self.reading.generation, tail).into_iter().enumerate() {
            let text = format!("{head}{text}");
            if !list.iter().any(|c| c.text == text) {
                list.push(Candidate { text, source: Source::Engine(EngineRank(engine_index)) });
            }
        }
        list
    }

    /// 取り消しや字種変換で読みに戻るとき、英語の判定は持ち越さない。
    fn leave_english(&mut self) {
        if self.typing == Typing::English {
            self.typing = Typing::Kana;
            if self.reading.list.is_empty() {
                // 英語を打っている間は変換していないので、読みに戻る前に変換しておく。
                self.reading.list = self.convert_reading();
            }
        }
    }

    /// 英単語のあとに助詞などが続く組成 (artifactni → artifact + に)。finished は打ち終えたとき。
    pub fn english_split(&self, finished: bool) -> Option<english_split::Split> {
        if !self.is_kana_editing() {
            return None;
        }
        english_split::split(
            &self.raw(),
            finished,
            |w| self.lexicon.knows_english(w),
            |w| self.lexicon.is_english_word(w),
            |w| self.lexicon.is_english_prefix(w),
            |kana| self.engine.has_reading(kana),
        )
    }

    /// 英単語を 1 つの単位にまとめ、かな混じり (Mixed) として確定・変換できるようにする。
    /// 変換候補は、かなの部分をエンジンで変換したものに英単語を前に付けたもの (artifactを使う)。
    fn fix_english_split(&mut self, split: english_split::Split) {
        let tail = split.tail_kana();
        let origin = (self.segments.clone(), self.romaji.buffer().to_string());
        self.pin = None;
        let head_text = split.head.clone();
        let head = Segment::Literal(split.head);
        let tail_segments = split.tail.into_iter().map(|(keys, kana)| Segment::Kana { keys, kana });
        self.segments = std::iter::once(head).chain(tail_segments).collect();
        self.romaji.clear();
        self.sync_hiragana();
        self.reset_candidates();
        self.stage = Stage::Editing;
        self.typing = Typing::Mixed;
        self.split_origin = Some(origin);
        self.reading.list = self.convert_mixed(&head_text, &tail);
    }

    /// 英語を打っていると見なせる局面。英字のまま表示し、確定も英字にする。
    /// かなの途中に変換できない子音が挟まった (merge の めrげ) か、かなにしきれない打鍵が残り、
    /// 打鍵列全体が英語として確定したことのある語 (git) か 4 文字以上の英単語 (enter) になっている。
    /// 表の短い語は日本語の打ちかけ (in, den) と紛れるので外す。
    pub fn is_typing_english(&self) -> bool {
        let raw = self.raw();
        let word = || !self.romaji.buffer().is_empty() && self.is_english_word_raw(&raw);
        self.is_kana_editing()
            && raw.bytes().all(|b| b.is_ascii_lowercase())
            && (self.has_ascii_kana() || word() || self.is_verdict(&raw, Verdict::Word) || self.is_verdict(&raw, Verdict::Judged))
    }

    /// 打鍵列全体が、未確定のローマ字もかなにしきれない文字も残らない英単語か。
    fn is_whole_word(&self, raw: &str) -> bool {
        self.romaji.buffer().is_empty() && !self.has_ascii_kana() && self.is_english_word_raw(raw)
    }

    /// 組成全体が英単語なら、英語として打つかを判定して持つ。そうでなければ持たない。
    /// かなの読みが辞書の見出しでない語 (issue → いっすえ) は、審判が読みの方をはっきり選ばない限り英語 (審判がなくても英語)。
    /// 見出しでもある語 (made, home) は、審判が英語を選んだときだけ英語。比べる相手は engine の第 1 候補。
    /// 固定中は表示と比べ、英語になるときだけ固定が外れる (`enter_english`)。raw が変わりうる所 (refresh / track_english / unpin) で呼ぶ。
    fn judge_word(&mut self) {
        let raw = self.raw();
        let eligible = self.is_kana_editing()
            && raw.bytes().all(|b| b.is_ascii_lowercase())
            && self.is_whole_word(&raw)
            && self.engine.has_dictionary();
        if !eligible {
            self.word_verdict = None;
            return;
        }
        // 比べる相手は engine の第 1 候補 (live の短い読みはひらがな表示でも同じ)。固定中は固定した分 + 残りの変換 (読みに戻すと見えている表示が変わる)。
        let converted = self.reading.list.first().filter(|c| c.engine_rank().is_some()).map(|c| c.text.clone());
        let first = match &self.pin {
            Some(pin) => Some(pin.surface() + converted.as_deref().unwrap_or_else(|| self.rest())),
            None => converted,
        };
        let verdict = if self.engine.has_reading(&self.hiragana) {
            match first {
                Some(over) if self.engine.prefers_text(&raw, &over) => Verdict::Judged,
                _ => Verdict::Kana,
            }
        } else {
            match first {
                Some(reading) if self.engine.rejects_english(&raw, &reading) => Verdict::Kana,
                _ => Verdict::Word,
            }
        };
        self.word_verdict = Some((raw, verdict));
    }

    /// 組成全体 (raw) の判定が `verdict` か。
    fn is_verdict(&self, raw: &str, verdict: Verdict) -> bool {
        matches!(&self.word_verdict, Some((r, v)) if r == raw && *v == verdict)
    }

    /// 打鍵列全体が英語として確定したことのある語か 4 文字以上の英単語か。
    fn is_english_word_raw(&self, raw: &str) -> bool {
        const MIN_LEN: usize = 4;
        self.lexicon.knows_english(raw) || (raw.len() >= MIN_LEN && self.lexicon.is_english_word(raw))
    }

    /// 変換できずに ASCII のまま残った文字が組成に含まれるか(英単語の合図)。英字の区切りは数えない。
    fn has_ascii_kana(&self) -> bool {
        self.segments.iter().any(|s| match s {
            Segment::Kana { kana, .. } => kana.chars().any(|c| c.is_ascii_alphabetic()),
            Segment::Literal(_) => false,
        })
    }

    /// 小文字 1 文字ぶんの打鍵をローマ字として受ける。
    pub fn type_romaji(&mut self, text: &str) {
        self.split_origin = None;
        let mut pending = self.romaji.buffer().to_string();
        pending.push_str(text);
        let kana: String = text
            .chars()
            .filter_map(|ch| self.romaji.process_input(ch.to_ascii_lowercase()))
            .collect();
        if kana.is_empty() {
            self.track_english();
            return;
        }
        // 確定した単位の打鍵は、ローマ字に残った未確定の打鍵 (`tt` の 2 つ目の t など) を除いたもの。
        let keys = pending.strip_suffix(self.romaji.buffer()).unwrap_or(&pending).to_string();
        self.push_kana(keys, &kana);
    }

    /// 数字・記号をそのままの打鍵として追加する (既定の和文表記があればそれをかなとして使う)。
    pub fn type_text(&mut self, text: &str) {
        debug_assert!(self.romaji.buffer().is_empty(), "romaji is flushed before text keys");
        let kana = symbols::default_form(text).unwrap_or(text);
        if kana.is_empty() {
            return;
        }
        // 矢印: `-` の直後の `>` は長音をやめて `->` にする。`<` の直後の `-` は長音にしない (`<-`)。
        let last_keys = match self.segments.last() {
            Some(Segment::Kana { keys, .. }) => Some(keys.as_str()),
            _ => None,
        };
        match (last_keys, text) {
            (Some("-"), ">") => {
                self.segments.pop();
                self.push_kana("->".to_string(), "->");
            }
            (Some("<"), "-") => self.push_kana(text.to_string(), text),
            _ => self.push_kana(text.to_string(), kana),
        }
    }

    fn push_kana(&mut self, keys: String, kana: &str) {
        self.segments.push(Segment::Kana { keys, kana: kana.to_string() });
        self.refresh();
    }

    /// 記号 1 文字をローマ字の続きとして受け取れるか (`n'` → ん)。
    pub fn accepts_romaji(&self, ch: char) -> bool {
        self.romaji.accepts(ch)
    }

    /// 未確定のローマ字があるか。
    pub fn has_romaji(&self) -> bool {
        !self.romaji.buffer().is_empty()
    }

    /// 未確定のローマ字を 1 文字消す。
    pub fn backspace_romaji(&mut self) {
        self.romaji.handle_backspace();
        self.track_english();
    }

    /// 未確定のローマ字を捨てる。
    pub fn clear_romaji(&mut self) {
        self.romaji.clear();
    }

    /// テスト用: 打鍵 raw と、それで確定したかな kana を追加する (kana が空なら raw は次に持ち越す)。
    /// carry は未確定のまま残る打鍵で、確定した単位には含めない。
    #[cfg(test)]
    pub fn push_input(&mut self, raw: &str, kana: &str, carry: &str) {
        let mut pending = self.romaji.buffer().to_string();
        pending.push_str(raw);
        self.romaji.clear();
        if kana.is_empty() {
            self.romaji.set_buffer(&pending);
            return;
        }
        let split = pending.strip_suffix(carry).map_or(pending.len(), str::len);
        self.romaji.set_buffer(&pending[split..]);
        self.push_kana(pending[..split].to_string(), kana);
    }

    /// テスト用: 外の RomajiConverter が返した Flush を取り込む。
    #[cfg(test)]
    pub fn push_flush(&mut self, flush: Flush) {
        let english = self.is_typing_english();
        let keys = self.romaji.buffer().to_string();
        self.romaji.clear();
        self.absorb_flush(english, keys, flush);
    }

    /// 英単語としてそのまま追加する。かな変換も候補生成もしない。
    pub fn push_literal(&mut self, text: &str) {
        self.pin = None;
        if !self.candidates().is_empty() {
            self.engine.cancel();
        }
        self.split_origin = None;
        let split = if self.typing == Typing::English { self.english_split(false) } else { None };
        if let Some(split) = split {
            // 英語分割を表示している間の大文字は、見えているとおり (英単語+かな) の後ろに付ける。
            self.segments = std::iter::once(Segment::Literal(split.head))
                .chain(split.tail.into_iter().map(|(keys, kana)| Segment::Kana { keys, kana }))
                .chain((!split.pending.is_empty()).then(|| Segment::Kana { keys: split.pending.clone(), kana: split.pending }))
                .chain(std::iter::once(Segment::Literal(text.to_string())))
                .collect();
            self.romaji.clear();
        } else if self.typing == Typing::English && self.english_shown && matches!(self.stage, Stage::Editing) {
            // 英語を打っている途中の大文字は、表示している英字の続きにする (かなには戻さない)。
            let word = self.raw() + text;
            self.romaji.clear();
            self.segments.clear();
            self.segments.push(Segment::Literal(word));
        } else {
            self.segments.push(Segment::Literal(text.to_string()));
        }
        self.sync_hiragana();
        self.reset_candidates();
        self.stage = Stage::Editing;
        self.typing = if self.segments.iter().all(Segment::is_literal) {
            Typing::Literal
        } else {
            Typing::Mixed
        };
    }

    /// live 変換で、underline に出ている (preedit と同じ) 第 1 候補の engine 順位。
    fn live_rank(&self) -> Option<EngineRank> {
        let live_mixed = matches!(self.stage, Stage::Editing) && self.typing == Typing::Mixed && self.live;
        if !self.is_live_kana() && !live_mixed {
            return None;
        }
        self.reading.list.first().and_then(Candidate::engine_rank)
    }

    /// live 変換が表示を変換する長さの読みか。かなだけを数え、未確定のローマ字・記号・数字・長音は数えない。
    fn live_converts(&self) -> bool {
        let kana = self.hiragana.chars().filter(|c| matches!(c, '\u{3041}'..='\u{3096}' | '\u{30A1}'..='\u{30FA}')).count();
        self.live && kana > LIVE_MIN_KANA
    }

    /// live 変換でかなを入力中で、英語としては扱われていない。短い読みはひらがなのままなので含めない。
    fn is_live_kana(&self) -> bool {
        matches!(self.stage, Stage::Editing)
            && self.typing == Typing::Kana
            && self.live_converts()
            && self.shown_split().is_none()
            && !self.is_typing_english()
    }

    /// live 変換の Backspace が消す読みの文字数と、消したあとに固定する語列。
    /// 見えている最後の 1 文字に当たる読みを、固定した語と残りの第 1 候補の語列から決める。当てはまらなければ None。
    fn live_drop(&self) -> Option<(usize, Vec<(String, String)>)> {
        if !self.is_live_kana() {
            return None;
        }
        let mut words = self.pin.as_ref().map_or_else(Vec::new, |pin| pin.words.clone());
        let mut displayed = self.pin.as_ref().map_or_else(String::new, Pin::surface);
        if !self.rest().is_empty() {
            let first = self.reading.list.first()?;
            let rank = first.engine_rank()?;
            let rest_words = self.engine.words(self.reading.generation, rank.0)?;
            words.extend(rest_words.into_iter().map(|(surface, ruby)| (surface.replace('\u{1F}', ""), ruby)));
            displayed.push_str(&first.text);
        }
        let refs: Vec<(&str, &str)> = words.iter().map(|(surface, ruby)| (surface.as_str(), ruby.as_str())).collect();
        let dropped =
            live_backspace::drop_len(&refs, &self.hiragana, &displayed, |suffix, kanji| self.engine.has_kanji_reading(suffix, kanji))?;
        words.pop();
        words.extend(dropped.kept);
        Some((dropped.len, words))
    }

    /// 末尾の読みを 1 文字消す。区切りが空になったら外す。
    fn pop_reading_char(&mut self) {
        let Some(last) = self.segments.last_mut() else {
            return;
        };
        let emptied = match last {
            Segment::Kana { keys, kana } => {
                kana.pop();
                *keys = kana.clone();
                kana.is_empty()
            }
            Segment::Literal(text) => {
                text.pop();
                text.is_empty()
            }
        };
        if emptied {
            self.segments.pop();
        }
    }

    pub fn backspace(&mut self) -> bool {
        if self.segments.is_empty() {
            return false;
        }
        self.split_origin = None;
        // live 変換中は、見えている最後の 1 文字に当たる読みを消し、残りは見えているまま固定する。分からなければ 1 文字。
        let dropped = self.live_drop();
        let rest_was_empty = self.rest().is_empty();
        for _ in 0..dropped.as_ref().map_or(1, |(count, _)| *count) {
            self.pop_reading_char();
        }
        match dropped {
            Some((_, words)) => self.pin = (!words.is_empty()).then_some(Pin { words }),
            // 固定した語どうしが合わないときは固定を外す。残りの変換が合わないときは固定を保つ。
            None if rest_was_empty => self.pin = None,
            None => {}
        }
        let was_literal = matches!(self.typing, Typing::Literal | Typing::Mixed);
        // 英単語の部分を消しきったら、残ったかなは通常の組成に戻す。
        if was_literal && self.segments.iter().any(Segment::is_literal) {
            self.pin = None;
            self.sync_hiragana();
            self.reset_candidates();
            self.typing = if self.segments.iter().all(Segment::is_literal) {
                Typing::Literal
            } else {
                Typing::Mixed
            };
        } else {
            self.refresh();
        }
        true
    }

    /// 最初の変換。保留中のリランキングを反映し、raw 候補を第 1 候補の直後に入れて先頭を選ぶ。
    pub fn begin_conversion(&mut self) -> bool {
        self.unpin();
        let mut kept = None;
        if matches!(self.stage, Stage::Transliterated { .. }) {
            if let Stage::Transliterated { kept: k, .. } = std::mem::replace(&mut self.stage, Stage::Editing) {
                kept = k;
            }
        }
        let phase = self.phase();
        if !matches!(
            phase,
            Phase::Composing | Phase::Reverted | Phase::Dangling | Phase::Mixed
        ) {
            return false;
        }
        if let Stage::Reverted { kept: k } = &mut self.stage {
            kept = k.take();
        }
        if self.typing == Typing::English && matches!(self.stage, Stage::Editing) && self.reading.list.is_empty() {
            // 英語を打っている間は変換していない。候補を求められたときに読みを変換する。
            self.reading.list = self.convert_reading();
        }
        let mut list = kept.unwrap_or_else(|| self.reading.list.clone());
        if phase == Phase::Mixed && !list.iter().any(|c| c.text == self.hiragana) {
            // 表示中のかな混じりを第 1 候補にし、raw はその直後に並べる。
            list.push(Candidate {
                text: self.hiragana.clone(),
                source: Source::MixedReading,
            });
        }
        self.insert_kana_fallback(&mut list, phase);
        // 英単語は raw より前に並べたいので、先に raw を入れてから第 1 候補の直後へ割り込ませる。
        self.insert_raw_candidate(&mut list, phase, true);
        self.insert_english_candidates(&mut list, phase);
        self.insert_emoji_candidates(&mut list, phase);
        // 英単語でない raw は絵文字の位置に影響しないよう、最後に末尾へ足す。
        self.insert_raw_candidate(&mut list, phase, false);
        if list.is_empty() {
            return false;
        }
        self.stage = Stage::Selecting(Selection { list, cursor: 0, window: false });
        true
    }

    /// 次の候補へ進み(末尾なら先頭へ戻り)、候補ウィンドウを出す状態にする。
    pub fn advance(&mut self) -> bool {
        let Stage::Selecting(s) = &mut self.stage else {
            return false;
        };
        s.cursor = (s.cursor + 1) % s.list.len();
        s.window = true;
        true
    }

    /// 前の候補へ戻り(先頭なら末尾へ回り)、候補ウィンドウを出す状態にする。
    pub fn retreat(&mut self) -> bool {
        let Stage::Selecting(s) = &mut self.stage else {
            return false;
        };
        s.cursor = (s.cursor + s.list.len() - 1) % s.list.len();
        s.window = true;
        true
    }

    pub fn select_candidate(&mut self, index: usize) -> bool {
        if index >= self.candidates().len() {
            return false;
        }
        if let Stage::Selecting(s) = &mut self.stage {
            s.cursor = index;
            s.window = true;
            return true;
        }
        let list = self.candidates().to_vec();
        self.stage = Stage::Selecting(Selection { list, cursor: index, window: true });
        true
    }

    pub fn select_candidate_text(&mut self, text: &str) -> bool {
        match self.candidates().iter().position(|c| c.text == text) {
            Some(index) => self.select_candidate(index),
            None => false,
        }
    }

    /// 組成を捨てて空に戻す(何も確定しない)。
    pub fn discard(&mut self) {
        self.engine.cancel();
        self.pin = None;
        self.segments.clear();
        self.split_origin = None;
        self.romaji.clear();
        self.hiragana.clear();
        self.reset_candidates();
        self.stage = Stage::Editing;
        self.typing = Typing::Empty;
        self.word_verdict = None;
    }

    pub fn revert(&mut self) {
        if self.phase() == Phase::Empty {
            return;
        }
        let kept = match std::mem::replace(&mut self.stage, Stage::Editing) {
            Stage::Selecting(s) => Some(s.list),
            Stage::Reverted { kept } | Stage::Transliterated { kept, .. } => kept,
            Stage::Editing => None,
        };
        self.unpin();
        // 英語分割でできた Mixed は、打った英字ではないので、分割前のかなの打鍵中に戻す (続きを打てば分割し直す)。
        if let Some((segments, pending)) = self.split_origin.take() {
            self.segments = segments;
            self.romaji.set_buffer(&pending);
            self.refresh();
            return;
        }
        // 英字入力 (Literal / Mixed) は局面を保って入力中に戻る。読みだけを表示する Reverted に入れるのはかなだけ。
        if matches!(self.typing, Typing::Literal | Typing::Mixed) {
            return;
        }
        self.leave_english();
        self.stage = Stage::Reverted { kept };
    }

    /// 確定する。学習は実行せず、何を学習すべきかを返すだけ。実行と可否の判定は Session が行う。
    pub fn commit(&mut self) -> Committed {
        let phase = self.phase();
        let text = self.commit_preview();
        let mut english = Vec::new();
        // 審判が英語と決めた分は学習しない (読みが見出しの語を、間違いごと固定しないため)。
        if phase == Phase::Dangling && !self.is_verdict(&self.raw(), Verdict::Judged) {
            english.push(text.clone());
        }
        english.extend(self.segments.iter().filter(|s| s.is_literal()).map(|s| s.keys().to_string()));
        // 固定した先頭部分を含む確定は、1 つの候補の順位では表せないので学習しない。
        let engine = match &self.stage {
            _ if self.pin.is_some() => None,
            Stage::Selecting(s) => s.list[s.cursor].engine_rank(),
            _ => self.live_rank(),
        }
        .map(|rank| (self.reading.generation, rank));
        self.pin = None;
        self.segments.clear();
        self.split_origin = None;
        self.romaji.clear();
        self.hiragana.clear();
        self.reset_candidates();
        self.stage = Stage::Editing;
        self.typing = Typing::Empty;
        self.word_verdict = None;
        Committed { text, engine, english }
    }

    /// 学習の実行 (engine.select / cancel) のために Session が使う。
    pub fn engine_mut(&mut self) -> &mut E {
        &mut self.engine
    }

    /// 組成全体がカタカナ語として引ける場合、英単語を第 1 候補の直後に入れる (raw はその後ろ)。
    fn insert_english_candidates(&self, list: &mut Vec<Candidate>, phase: Phase) {
        if matches!(phase, Phase::Dangling | Phase::Mixed) || self.is_symbol() {
            return;
        }
        let words = self.lexicon.english_for(&translit::to_katakana(&self.hiragana));
        let fresh: Vec<Candidate> = words
            .into_iter()
            .filter(|w| !list.iter().any(|c| &c.text == w))
            .map(|text| Candidate {
                text,
                source: Source::Loanword,
            })
            .collect();
        let position = list.len().min(1);
        list.splice(position..position, fresh);
    }

    /// 組成全体が絵文字の読みと一致する場合、絵文字を最大 EMOJI_LIMIT 件入れる。
    fn insert_emoji_candidates(&self, list: &mut Vec<Candidate>, phase: Phase) {
        if matches!(phase, Phase::Dangling | Phase::Mixed) || self.is_symbol() {
            return;
        }
        let fresh: Vec<Candidate> = self.lexicon.emoji_for(&self.hiragana)
            .into_iter()
            .filter(|e| !list.iter().any(|c| &c.text == e))
            .take(EMOJI_LIMIT)
            .map(|text| Candidate {
                text,
                source: Source::Emoji,
            })
            .collect();
        Self::place_emoji(list, fresh);
    }

    /// 絵文字を、上位 3 件のエンジン候補と英単語・raw の後ろに入れる (3 番目より前には入れない)。
    fn place_emoji(list: &mut Vec<Candidate>, emoji: Vec<Candidate>) {
        let engine = list.iter().filter(|c| c.engine_rank().is_some()).count();
        let extras = list.len() - engine;
        let position = (extras + engine.min(EMOJI_AFTER_ENGINE)).max(3).min(list.len());
        list.splice(position..position, emoji);
    }

    /// エンジンが候補を 1 つも返さなかったとき (辞書が開けない・未知の読みなど) に、ひらがなとカタカナを候補にする。
    /// これがないと raw (ローマ字) が唯一の候補になり、Space+Enter で日本語がローマ字に化ける。
    fn insert_kana_fallback(&self, list: &mut Vec<Candidate>, phase: Phase) {
        let unconverted = list.iter().all(|c| c.engine_rank().is_none());
        if !unconverted || self.is_symbol() || !matches!(phase, Phase::Composing | Phase::Reverted) {
            return;
        }
        let forms = [
            (self.hiragana.clone(), Source::Hiragana),
            (translit::to_katakana(&self.hiragana), Source::Katakana),
        ];
        for (text, source) in forms {
            if !text.is_empty() && !list.iter().any(|c| c.text == text) {
                list.push(Candidate { text, source });
            }
        }
    }

    /// raw を入れる。Dangling は先頭、英単語なら第 2 候補 (`early` の回)、それ以外は末尾 (`!early` の回)。
    /// 英単語でない raw (kazu, dei) は候補としてはノイズなので末尾に回す (消さない)。
    fn insert_raw_candidate(&self, list: &mut Vec<Candidate>, phase: Phase, early: bool) {
        let raw = self.raw();
        if raw.is_empty() || self.is_symbol() || list.iter().any(|c| c.text == raw) {
            return;
        }
        let is_early = phase == Phase::Dangling || self.is_english_word_raw(&raw);
        if is_early != early {
            return;
        }
        let position = match phase {
            Phase::Dangling => 0,
            _ if early => list.len().min(1),
            _ => list.len(),
        };
        list.insert(
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
    use crate::deps::FakeLexicon;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Clone, Default)]
    struct Log {
        converts: Rc<RefCell<Vec<(u64, String)>>>,
        selects: Rc<RefCell<Vec<(u64, usize)>>>,
        cancels: Rc<RefCell<usize>>,
        /// 表の読みのほかに、辞書の見出しとして答える読み。
        headwords: Rc<RefCell<Vec<String>>>,
        /// `prefers_text` の答え。
        prefers: Rc<RefCell<bool>>,
        /// `rejects_english` の答え。
        rejects: Rc<RefCell<bool>>,
        /// true なら辞書を開けていない engine として答える。
        no_dictionary: Rc<RefCell<bool>>,
        /// `prefers_text` に渡された (text, over)。
        asked: Rc<RefCell<Vec<(String, String)>>>,
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
        /// 表にある読みと `Log::headwords` の読みが辞書の見出し。
        fn has_reading(&self, hiragana: &str) -> bool {
            self.table.iter().any(|(k, _)| k == hiragana) || self.log.headwords.borrow().iter().any(|h| h == hiragana)
        }
        fn rejects_english(&mut self, _raw: &str, _reading: &str) -> bool {
            *self.log.rejects.borrow()
        }
        fn has_dictionary(&self) -> bool {
            !*self.log.no_dictionary.borrow()
        }
        fn prefers_text(&mut self, text: &str, over: &str) -> bool {
            self.log.asked.borrow_mut().push((text.to_string(), over.to_string()));
            *self.log.prefers.borrow()
        }
    }

    impl Composer<MockEngine, FakeLexicon> {
        fn engine_cancels(&self) -> std::cell::Ref<'_, usize> {
            self.engine.log.cancels.borrow()
        }
    }

    /// Session が学習を許可して確定するときと同じ engine の扱い (select か cancel)。
    fn commit_learning(c: &mut Composer<MockEngine, FakeLexicon>) -> String {
        let Committed { text, engine, .. } = c.commit();
        match engine {
            Some((generation, EngineRank(i))) => {
                c.engine_mut().select(generation, i);
            }
            None => c.engine_mut().cancel(),
        }
        text
    }

    fn composer() -> (Composer<MockEngine, FakeLexicon>, Log) {
        let log = Log::default();
        let engine = MockEngine {
            log: log.clone(),
            table: vec![(
                "きょう".to_string(),
                vec!["今日".to_string(), "京".to_string(), "教".to_string()],
            )],
        };
        // kyou は raw が第 2 候補に残る英単語として扱う (非英単語の raw は末尾になるため)。
        let lexicon = FakeLexicon::default().with_english_word(|w| w == "kyou");
        (Composer::new(engine, lexicon), log)
    }

    fn type_kyou(c: &mut Composer<MockEngine, FakeLexicon>) {
        c.push_input("k", "", "");
        c.push_input("yo", "きょ", "");
        c.push_input("u", "う", "");
    }

    fn texts(c: &Composer<MockEngine, FakeLexicon>) -> Vec<&str> {
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
        let mut c = Composer::new(engine, FakeLexicon::default());
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
        let mut c = Composer::new(engine, FakeLexicon::default());
        c.push_input("gi", "ぎ", "");
        assert!(c.begin_conversion());
        assert_eq!(c.display(), "ぎ");
        assert_eq!(texts(&c), vec!["ぎ", "ギ", "gi"]);
        assert!(c.candidates().iter().all(|cand| cand.engine_rank().is_none()));
        assert_eq!(commit_learning(&mut c), "ぎ");
        assert_eq!(*c.engine_cancels(), 1, "学習しない");
    }

    #[test]
    fn 英単語でないrawは末尾に回り消えない() {
        let engine = MockEngine {
            log: Log::default(),
            table: vec![("かず".to_string(), vec!["数".to_string(), "和".to_string()])],
        };
        let mut c = Composer::new(engine, FakeLexicon::default().with_english_word(|w| w == "kyou"));
        c.push_input("kazu", "かず", "");
        assert!(c.begin_conversion());
        assert_eq!(texts(&c), vec!["数", "和", "kazu"]);
        assert_eq!(c.candidates()[2].source, Source::Raw);
    }

    #[test]
    fn 英単語のrawは第2候補に残る() {
        let engine = MockEngine {
            log: Log::default(),
            table: vec![("かず".to_string(), vec!["数".to_string(), "和".to_string()])],
        };
        let mut c = Composer::new(engine, FakeLexicon::default().with_english_word(|w| w == "kazu"));
        c.push_input("kazu", "かず", "");
        assert!(c.begin_conversion());
        assert_eq!(texts(&c), vec!["数", "kazu", "和"]);
    }

    #[test]
    fn 変換候補が空でもdanglingならrawが先頭のまま() {
        let engine = MockEngine { log: Log::default(), table: vec![] };
        let mut c = Composer::new(engine, FakeLexicon::default());
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
        c.backspace_romaji();
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
        let mut c = Composer::new(engine, FakeLexicon::default());
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

    /// 語の情報を持つ engine のつもり。表にある読みはその語列を、無い読みは「変」を付けた 1 語を返す。
    #[derive(Default)]
    struct WordsEngine {
        last: Option<(u64, Vec<Vec<(String, String)>>)>,
        /// `words` を呼ばれた回数。
        asked: Rc<RefCell<usize>>,
        /// false なら `words` は常に None (語の情報を持たない engine)。
        has_words: bool,
        /// `convert` に渡された読み。
        converted: Rc<RefCell<Vec<String>>>,
        /// `cancel` が呼ばれた回数。
        cancels: Rc<RefCell<usize>>,
    }

    fn pairs(words: &[(&str, &str)]) -> Vec<(String, String)> {
        words.iter().map(|(surface, ruby)| (surface.to_string(), ruby.to_string())).collect()
    }

    fn words_table(hiragana: &str) -> Vec<Vec<(String, String)>> {
        match hiragana {
            "せんせい" => vec![pairs(&[("先生", "センセイ")]), pairs(&[("戦", "セン"), ("世以", "セイ")])],
            "せんせいは" => vec![pairs(&[("先生", "センセイ"), ("は", "ハ")])],
            "せん" => vec![pairs(&[("戦", "セン")])],
            "こう" => vec![pairs(&[("後", "コウ")])],
            "くぎをうつ" => vec![pairs(&[("釘", "クギ"), ("を", "ヲ"), ("打つ", "ウツ")])],
            "かいしゃのけん" => vec![pairs(&[("会社", "カイシャ"), ("の件", "ノケン")])],
            // 語列が読みと合わない engine (学習・補正などで組み立てが違う)。
            "ほ" | "ほげ" => vec![pairs(&[("保", "ホホ")])],
            _ => vec![pairs(&[(&format!("変{hiragana}"), &translit::to_katakana(hiragana))])],
        }
    }

    impl CandidateEngine for WordsEngine {
        fn convert(&mut self, revision: u64, hiragana: &str) -> Vec<String> {
            self.converted.borrow_mut().push(hiragana.to_string());
            if hiragana.is_empty() {
                return Vec::new();
            }
            let table = words_table(hiragana);
            let texts = table.iter().map(|words| words.iter().map(|(surface, _)| surface.as_str()).collect()).collect();
            self.last = Some((revision, table));
            texts
        }
        fn select(&mut self, _revision: u64, _engine_index: usize) -> bool {
            true
        }
        fn cancel(&mut self) {
            *self.cancels.borrow_mut() += 1;
        }
        fn words(&self, revision: u64, index: usize) -> Option<Vec<(String, String)>> {
            *self.asked.borrow_mut() += 1;
            let (r, table) = self.last.as_ref()?;
            (self.has_words && *r == revision).then(|| table.get(index).cloned()).flatten()
        }
        fn has_kanji_reading(&self, katakana: &str, kanji: char) -> bool {
            matches!((katakana, kanji), ("シャ", '社') | ("セイ", '生') | ("ケン", '件'))
        }
    }

    type WordsComposer = Composer<WordsEngine, FakeLexicon>;

    fn words_composer() -> WordsComposer {
        let mut c = Composer::new(WordsEngine { has_words: true, ..WordsEngine::default() }, FakeLexicon::default());
        c.set_live(true);
        c
    }

    fn type_units(c: &mut WordsComposer, units: &[(&str, &str)]) {
        for (raw, kana) in units {
            c.push_input(raw, kana, "");
        }
    }

    fn type_sensei(c: &mut WordsComposer) {
        type_units(c, &[("se", "せ"), ("n", "ん"), ("se", "せ"), ("i", "い")]);
    }

    fn type_kaisha(c: &mut WordsComposer) {
        type_units(c, &[("ka", "か"), ("i", "い"), ("sha", "しゃ")]);
    }

    fn type_kou(c: &mut WordsComposer) {
        type_units(c, &[("ko", "こ"), ("u", "う")]);
    }

    /// 先生は を打って Backspace で 先生 (読みはせんせい) を固定した状態。
    fn pinned_sensei() -> WordsComposer {
        let mut c = words_composer();
        type_sensei(&mut c);
        type_units(&mut c, &[("ha", "は")]);
        assert!(c.backspace());
        c
    }

    fn assert_wysiwyg(c: &WordsComposer) {
        assert_eq!(c.preedit(), c.commit_preview());
    }

    #[test]
    fn live中のbackspaceは見えている最後の文字に当たる読みを消し残りを固定する() {
        let mut c = pinned_sensei();
        assert_eq!(c.hiragana, "せんせい");
        assert_eq!(c.raw(), "sensei");
        assert_eq!(c.display(), "先生");
        assert_eq!(c.preedit(), "先生");
        assert_wysiwyg(&c);
    }

    #[test]
    fn 固定した先のbackspaceでさらに消すと空になる() {
        let mut c = pinned_sensei();
        while !c.is_empty() {
            assert!(c.backspace());
            assert_wysiwyg(&c);
        }
        assert_eq!(c.phase(), Phase::Empty);
        assert!(c.is_empty());
        assert_eq!(c.display(), "");
        assert!(c.pin.is_none());
    }

    #[test]
    fn 釘を打つのbackspaceは打を残して固定する() {
        let mut c = words_composer();
        type_units(&mut c, &[("ku", "く"), ("gi", "ぎ"), ("wo", "を"), ("u", "う"), ("tu", "つ")]);
        assert_eq!(c.display(), "釘を打つ");
        assert!(c.backspace());
        assert_eq!(c.hiragana, "くぎをう");
        assert_eq!(c.display(), "釘を打");
        assert_wysiwyg(&c);
    }

    #[test]
    fn 会社の件をbackspaceで3回消すと会だけが残る() {
        let mut c = words_composer();
        type_units(&mut c, &[("ka", "か"), ("i", "い"), ("sha", "しゃ"), ("no", "の"), ("ke", "け"), ("n", "ん")]);
        assert_eq!(c.display(), "会社の件");
        let mut shown = Vec::new();
        for _ in 0..3 {
            assert!(c.backspace());
            shown.push(c.display().into_owned());
            assert_wysiwyg(&c);
        }
        // 読みが 2 かな (かい) になったら固定せずひらがなに戻る。
        assert_eq!(shown, ["会社の", "会社", "かい"]);
        assert_eq!(c.hiragana, "かい");
    }

    #[test]
    fn 固定したあとにかなを打つと残りの読みだけを変換して後ろに付ける() {
        let mut c = pinned_sensei();
        c.engine.converted.borrow_mut().clear();
        type_kou(&mut c);
        assert_eq!(c.hiragana, "せんせいこう");
        assert_eq!(c.engine.converted.borrow().as_slice(), ["こ", "こう"]);
        assert_eq!(c.display(), "先生後");
        assert_eq!(c.preedit(), "先生後");
        assert_wysiwyg(&c);
    }

    #[test]
    fn 固定中に未確定のローマ字があれば固定と残りの後ろに出る() {
        let mut c = pinned_sensei();
        c.type_romaji("k");
        assert_eq!(c.preedit(), "先生k");
        assert!(c.has_romaji());
    }

    #[test]
    fn 固定した後の残りがあるときのbackspaceは固定を保ち残りを固定に足す() {
        let mut c = pinned_sensei();
        type_kou(&mut c);
        assert!(c.backspace());
        // 後 (こう) は 1 字なので読みごと消え、固定は 先生 のまま。
        assert_eq!(c.hiragana, "せんせい");
        assert_eq!(c.display(), "先生");
        assert_wysiwyg(&c);
    }

    #[test]
    fn 残りの語列が合わないbackspaceは残りを1文字だけ消して固定を保つ() {
        let mut c = pinned_sensei();
        type_units(&mut c, &[("ho", "ほ"), ("ge", "げ")]);
        assert_eq!(c.display(), "先生保");
        assert!(c.backspace());
        assert_eq!(c.hiragana, "せんせいほ");
        assert_eq!(c.display(), "先生保");
        assert_eq!(c.pin.as_ref().map(Pin::surface).as_deref(), Some("先生"));
    }

    #[test]
    fn 固定中の確定は表示のまま学習しない() {
        let mut c = pinned_sensei();
        let committed = c.commit();
        assert_eq!(committed.text, "先生");
        assert_eq!(committed.engine, None);
        assert!(c.pin.is_none());
        assert!(c.is_empty());
    }

    #[test]
    fn 固定と残りを含む確定は表示のまま学習しない() {
        let mut c = pinned_sensei();
        type_kou(&mut c);
        let preview = c.commit_preview();
        let committed = c.commit();
        assert_eq!(committed.text, "先生後");
        assert_eq!(preview, "先生後");
        assert_eq!(committed.engine, None);
    }

    #[test]
    fn 固定中のスペースは固定を外して読み全体を変換し直す() {
        let mut c = pinned_sensei();
        assert!(c.begin_conversion());
        assert!(c.pin.is_none());
        assert_eq!(c.hiragana, "せんせい");
        assert_eq!(c.display(), "先生");
        assert_eq!(c.candidates().first().map(|x| x.text.as_str()), Some("先生"));
        assert_eq!(c.engine.converted.borrow().last().map(String::as_str), Some("せんせい"));
    }

    #[test]
    fn 固定中のescapeは読みごと捨てる() {
        let mut c = pinned_sensei();
        c.discard();
        assert!(c.is_empty());
        assert!(c.pin.is_none());
        assert_eq!(c.display(), "");
    }

    #[test]
    fn 固定中に読みへ戻すと固定が外れて読みが残る() {
        let mut c = pinned_sensei();
        c.revert();
        assert!(c.pin.is_none());
        assert_eq!(c.phase(), Phase::Reverted);
        assert_eq!(c.display(), "せんせい");
        assert!(c.begin_conversion());
        assert_eq!(c.candidates().first().map(|x| x.text.as_str()), Some("先生"));
    }

    #[test]
    fn 固定中の字種変換は読み全体を変換し固定を外す() {
        let mut c = pinned_sensei();
        assert!(c.transliterate(Form::Katakana));
        assert!(c.pin.is_none());
        assert_eq!(c.display(), "センセイ");
        assert_eq!(c.commit_preview(), "センセイ");
    }

    #[test]
    fn 固定中に大文字を打つと固定が外れてかな混じりになる() {
        let mut c = pinned_sensei();
        c.push_literal("A");
        assert!(c.pin.is_none());
        assert_eq!(c.phase(), Phase::Mixed);
        assert_eq!(c.hiragana, "せんせいA");
        assert_eq!(c.preedit(), "せんせいA");
        assert_wysiwyg(&c);
    }

    #[test]
    fn 固定中にliveを切ると固定が外れて読みが残る() {
        let mut c = pinned_sensei();
        c.set_live(false);
        assert!(c.pin.is_none());
        assert_eq!(c.display(), "せんせい");
        assert_eq!(c.preedit(), "せんせい");
        assert_wysiwyg(&c);
    }

    #[test]
    fn 固定中は候補を出さず選択もできない() {
        let mut c = pinned_sensei();
        assert!(c.candidates().is_empty());
        assert!(!c.select_candidate(0));
        assert!(!c.select_candidate_text("先生"));
        assert!(c.pin.is_some());
        assert_eq!(c.display(), "先生");
        assert_wysiwyg(&c);
    }

    #[test]
    fn 固定中のliveではbackspaceのたびに表示と確定が一致する() {
        let mut c = words_composer();
        type_units(&mut c, &[("ka", "か"), ("i", "い"), ("sha", "しゃ"), ("no", "の"), ("ke", "け"), ("n", "ん")]);
        while !c.is_empty() {
            assert!(c.backspace());
            assert_wysiwyg(&c);
        }
    }

    #[test]
    fn live中のbackspaceが単位の途中で切れても残りの打鍵はrawに反映され英語判定にならない() {
        let mut c = words_composer();
        type_kaisha(&mut c);
        assert!(c.backspace());
        assert_eq!(c.hiragana, "かいし");
        assert_eq!(c.display(), "変かいし");
        assert_eq!(c.raw(), "kaiし");
        assert!(!c.is_typing_english());
        assert_eq!(c.phase(), Phase::Composing);
        assert!(c.begin_conversion());
        let raw: Vec<&str> = c.candidates().iter().filter(|cand| cand.source == Source::Raw).map(|cand| cand.text.as_str()).collect();
        assert_eq!(raw, ["kaiし"]);
    }

    #[test]
    fn engineが語を返さなければ1文字だけ消して固定しない() {
        let mut c = Composer::new(WordsEngine::default(), FakeLexicon::default());
        c.set_live(true);
        type_kaisha(&mut c);
        assert!(c.backspace());
        assert_eq!(*c.engine.asked.borrow(), 1);
        assert_eq!(c.hiragana, "かいし");
        assert!(c.pin.is_none());
    }

    #[test]
    fn 非liveのbackspaceはengineに聞かず1文字だけ消す() {
        let mut c = words_composer();
        c.set_live(false);
        type_kaisha(&mut c);
        assert!(c.backspace());
        assert_eq!(*c.engine.asked.borrow(), 0);
        assert_eq!(c.hiragana, "かいし");
    }

    #[test]
    fn 読みに戻した後のbackspaceはengineに聞かず1文字だけ消す() {
        let mut c = words_composer();
        type_kaisha(&mut c);
        c.revert();
        assert!(c.backspace());
        assert_eq!(*c.engine.asked.borrow(), 0);
        assert_eq!(c.hiragana, "かいし");
    }

    #[test]
    fn liveでも記号1つの読みは1文字だけ消してengineに聞かない() {
        let mut c = words_composer();
        c.push_input("[", "「", "");
        assert!(c.is_symbol());
        assert!(c.backspace());
        assert_eq!(*c.engine.asked.borrow(), 0);
        assert!(c.is_empty());
    }

    #[test]
    fn live中でも読みが2かな以下ならひらがなのまま確定する() {
        let mut c = words_composer();
        type_units(&mut c, &[("se", "せ"), ("n", "ん")]);
        assert_eq!(c.display(), "せん");
        assert_eq!(c.preedit(), "せん");
        assert_wysiwyg(&c);
        let committed = c.commit();
        assert_eq!(committed.text, "せん");
        assert_eq!(committed.engine, None);
    }

    #[test]
    fn live中の記号と数字は読みの長さに数えない() {
        for tail in ["!", "?", "1"] {
            let mut c = words_composer();
            type_units(&mut c, &[("se", "せ"), ("n", "ん")]);
            c.push_input(tail, tail, "");
            c.push_input(tail, tail, "");
            assert_eq!(c.preedit(), format!("せん{tail}{tail}"));
            assert_wysiwyg(&c);
        }
    }

    #[test]
    fn live中の未確定のローマ字は読みの長さに数えない() {
        let mut c = words_composer();
        type_units(&mut c, &[("se", "せ"), ("n", "ん")]);
        c.type_romaji("k");
        assert_eq!(c.preedit(), "せんk");
    }

    #[test]
    fn live中でも読みが3かなになると変換して表示する() {
        let mut c = words_composer();
        type_units(&mut c, &[("se", "せ"), ("n", "ん"), ("se", "せ")]);
        assert_eq!(c.display(), "変せんせ");
        assert_wysiwyg(&c);
    }

    #[test]
    fn live中の短い読みでもスペースで候補を出せる() {
        let mut c = words_composer();
        type_units(&mut c, &[("se", "せ"), ("n", "ん")]);
        assert!(c.begin_conversion());
        assert_eq!(c.display(), "戦");
        assert_eq!(c.candidates().first().map(|x| x.text.as_str()), Some("戦"));
    }

    #[test]
    fn live中のbackspaceで3かなから2かなに戻ると2かなを直接打った表示と同じになる() {
        let mut c = words_composer();
        type_units(&mut c, &[("se", "せ"), ("n", "ん"), ("se", "せ")]);
        assert!(c.backspace());
        let mut direct = words_composer();
        type_units(&mut direct, &[("se", "せ"), ("n", "ん")]);
        assert_eq!(c.preedit(), "せん");
        assert_eq!(c.preedit(), direct.preedit());
        assert_eq!(c.commit_preview(), direct.commit_preview());
        assert!(c.pin.is_none());
        assert_wysiwyg(&c);
    }

    #[test]
    fn 非liveでは読みの長さに関わらずひらがなのまま() {
        let mut c = words_composer();
        c.set_live(false);
        type_units(&mut c, &[("se", "せ"), ("n", "ん"), ("se", "せ")]);
        assert_eq!(c.display(), "せんせ");
    }

    #[test]
    fn 固定していないlive中の確定は第1候補を学習対象にできる() {
        let mut c = words_composer();
        type_kaisha(&mut c);
        let committed = c.commit();
        assert_eq!(committed.text, "変かいしゃ");
        assert_eq!(committed.engine.map(|(_, rank)| rank), Some(EngineRank(0)));
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
        // 「は」は助詞なので、mock の語彙だと kyou + は の英語分割になる。分割にならない「き」を使う。
        c.push_input("ki", "き", "");
        assert_eq!(c.phase(), Phase::Composing);
        assert_eq!(c.display(), "きょうきK1");
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
        assert_eq!(commit_learning(&mut c), "きょう");
        assert_eq!(*log.cancels.borrow(), 1);
        assert!(log.selects.borrow().is_empty());
        assert!(c.is_empty());
        assert_eq!(c.raw(), "");
    }

    #[test]
    fn 選択中の候補をcommitしその元indexで学習する() {
        let (mut c, log) = composer();
        type_kyou(&mut c);
        let generation = c.reading.generation;
        c.begin_conversion();
        assert_eq!(commit_learning(&mut c), "今日");
        assert_eq!(*log.selects.borrow(), vec![(generation, 0)]);

        type_kyou(&mut c);
        c.begin_conversion();
        c.select_candidate(2);
        assert_eq!(commit_learning(&mut c), "京");
        assert_eq!(log.selects.borrow().last().unwrap().1, 1);
    }

    #[test]
    fn raw候補をcommitしても学習せずcancelする() {
        let (mut c, log) = composer();
        type_kyou(&mut c);
        c.begin_conversion();
        c.advance();
        assert_eq!(commit_learning(&mut c), "kyou");
        assert!(log.selects.borrow().is_empty());
        assert_eq!(*log.cancels.borrow(), 1);
    }

    #[test]
    fn liveで変換前にcommitすると第1候補を学習して返す() {
        let (mut c, log) = composer();
        c.set_live(true);
        type_kyou(&mut c);
        assert_eq!(commit_learning(&mut c), "今日");
        assert_eq!(log.selects.borrow().last().unwrap().1, 0);
    }

    #[test]
    fn revert後のcommitは生ひらがなでcancelする() {
        let (mut c, log) = composer();
        c.set_live(true);
        type_kyou(&mut c);
        c.revert();
        assert_eq!(commit_learning(&mut c), "きょう");
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
        assert_eq!(commit_learning(&mut c), "Git");
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

    use crate::keymap::{action_for, Action, Key};
    use crate::romaji_converter::RomajiConverter;

    fn type_word(c: &mut Composer<MockEngine, FakeLexicon>, conv: &mut RomajiConverter, word: &str) {
        for ch in word.chars() {
            let kana: String = conv.process_input(ch).unwrap_or_default();
            c.push_input(&ch.to_string(), &kana, conv.buffer());
        }
    }

    fn typed_and_flushed(word: &str) -> Composer<MockEngine, FakeLexicon> {
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
        assert_eq!(commit_learning(&mut c), "enter");
    }

    #[test]
    fn 変換しきれない子音が残る語はspaceでrawが第1候補になりかな候補が続く() {
        let mut c = typed_and_flushed("github");
        assert!(c.begin_conversion());
        assert_eq!(c.phase(), Phase::Converting { selected: 0, window: false });
        assert_eq!(c.display(), "github");
        assert_eq!(texts(&c)[0], "github");
        assert!(texts(&c).len() >= 2);
        assert_eq!(commit_learning(&mut c), "github");
        assert_eq!(*c.engine_cancels(), 1);
    }

    #[test]
    fn 末尾のnはかなのまま確定する() {
        let mut c = typed_and_flushed("kan");
        assert_eq!(c.phase(), Phase::Composing);
        assert_eq!(commit_learning(&mut c), "かん");
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
        assert_eq!(format!("{}{}", commit_learning(&mut c), "."), "enter.");
    }

    #[test]
    fn 子音が残る語のbackspace後も残りがあれば英単語のまま() {
        let (mut c, _) = composer();
        let mut conv = RomajiConverter::new();
        type_word(&mut c, &mut conv, "entery");
        conv.handle_backspace();
        c.backspace_romaji();
        c.push_flush(conv.flush());
        assert_eq!(c.phase(), Phase::Dangling);
        assert_eq!(commit_learning(&mut c), "enter");
    }

    fn type_symbol(c: &mut Composer<MockEngine, FakeLexicon>, ascii: &str) {
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
        assert_eq!(commit_learning(&mut c), "「");
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
        assert_eq!(commit_learning(&mut c), "『");
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
        assert_eq!(commit_learning(&mut c), "．");
    }

    #[test]
    fn 記号の後にローマ字を続けると辞書変換の組成になる() {
        let (mut c, log) = composer();
        let mut conv = RomajiConverter::new();
        type_symbol(&mut c, "[");
        type_word(&mut c, &mut conv, "kagi");
        type_symbol(&mut c, "]");
        assert_eq!(c.hiragana, "「かぎ」");
        assert!(!c.is_symbol());
        assert_eq!(c.raw(), "[kagi]");
        assert_eq!(log.converts.borrow().last().map(|(_, h)| h.as_str()), Some("「かぎ」"));
        assert!(c.begin_conversion());
        assert_eq!(c.candidates()[0].text, "「かぎ」K1");
        assert_eq!(texts(&c).last().copied(), Some("[kagi]"), "英単語でない raw は末尾");
    }

    #[test]
    fn 記号の直後の1文字だけのローマ字でも組成が続く() {
        let (mut c, _) = composer();
        let mut conv = RomajiConverter::new();
        type_symbol(&mut c, "(");
        type_word(&mut c, &mut conv, "a");
        c.push_flush(conv.flush());
        assert_eq!(c.hiragana, "（あ");
        assert_eq!(commit_learning(&mut c), "（あ");
    }

    #[test]
    fn 記号を並べた組成は辞書変換に任せる() {
        let (mut c, _) = composer();
        type_symbol(&mut c, "[");
        type_symbol(&mut c, "]");
        assert!(!c.is_symbol());
        assert_eq!(c.hiragana, "「」");
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
        assert_eq!(commit_learning(&mut c), "world");
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
        assert_eq!(commit_learning(&mut c), "ｋｙｏｕ");
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
    fn 取り消しても_mixed_のままで再変換は同じ候補列になる() {
        let (mut c, _) = composer();
        type_kyou(&mut c);
        c.push_literal("T");
        assert!(c.begin_conversion());
        assert_eq!(texts(&c), vec!["きょうT", "kyouT"]);
        c.revert();
        assert_eq!(c.phase(), Phase::Mixed);
        assert_eq!(c.display(), "きょうT");
        assert!(c.begin_conversion());
        assert_eq!(texts(&c), vec!["きょうT", "kyouT"]);
    }

    #[test]
    fn 取り消しても_literal_のままで確定は英字になる() {
        let (mut c, _) = composer();
        c.push_literal("Tokyo");
        c.transliterate(Form::Katakana);
        c.revert();
        assert_eq!(c.phase(), Phase::Literal);
        assert_eq!(commit_learning(&mut c), "Tokyo");
    }

    #[test]
    fn 取り消しからの再変換は直前の候補列から続く_dangling() {
        let engine = MockEngine { log: Log::default(), table: vec![("きょうh".to_string(), vec![])] };
        let mut c = Composer::new(engine, FakeLexicon::default());
        c.push_flush(Flush::Dangling("きょうh".to_string()));
        c.push_input("kyouh", "", "");
        assert!(c.begin_conversion());
        assert_eq!(texts(&c), vec!["kyouh"]);
        c.revert();
        assert!(c.begin_conversion());
        assert_eq!(texts(&c), vec!["kyouh", "きょうh", "キョウh"]);
    }

    #[test]
    fn 字種変換から変換へ戻ると_変換中だった候補列から続く() {
        let (mut c, _) = composer();
        type_kyou(&mut c);
        c.begin_conversion();
        c.advance();
        assert!(c.transliterate(Form::Katakana));
        assert_eq!(texts(&c), vec!["今日", "kyou", "京", "教"]);
        assert!(c.begin_conversion());
        assert_eq!(c.phase(), Phase::Converting { selected: 0, window: false });
        assert_eq!(texts(&c), vec!["今日", "kyou", "京", "教"]);
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

    fn live_split_composer() -> (Composer<MockEngine, FakeLexicon>, Log) {
        let log = Log::default();
        let engine = MockEngine { log: log.clone(), table: vec![("を".to_string(), vec!["を".to_string()])] };
        let mut c = Composer::new(engine, FakeLexicon::default().with_english_word(|w| w == "artifact"));
        c.set_live(true);
        (c, log)
    }

    fn type_keys(c: &mut Composer<MockEngine, FakeLexicon>, keys: &str) {
        for ch in keys.chars() {
            c.type_romaji(&ch.to_string());
        }
    }

    #[test]
    fn live中の英語分割はかな部分を変換して英単語に続けて表示する() {
        let (mut c, _) = live_split_composer();
        type_keys(&mut c, "artifactwo");
        assert_eq!(c.preedit(), "artifactを");
        type_keys(&mut c, "ko");
        assert_eq!(c.preedit(), "artifactをこK1");
        assert_eq!(c.preedit(), c.commit_preview());
    }

    #[test]
    fn live中の英語分割は未確定のローマ字だけが動く打鍵では変換し直さない() {
        let (mut c, log) = live_split_composer();
        type_keys(&mut c, "artifactwok");
        let converts = log.converts.borrow().len();
        assert_eq!(c.preedit(), "artifactをk");
        type_keys(&mut c, "y");
        assert_eq!(c.preedit(), "artifactをky");
        assert_eq!(log.converts.borrow().len(), converts);
    }

    #[test]
    fn live中の英語分割はenterで表示どおりに確定し学習する() {
        let (mut c, log) = live_split_composer();
        type_keys(&mut c, "artifactwoko");
        let shown = c.preedit();
        c.flush_romaji();
        assert_eq!(c.phase(), Phase::Mixed);
        assert_eq!(commit_learning(&mut c), shown);
        assert_eq!(shown, "artifactをこK1");
        assert_eq!(log.selects.borrow().len(), 1);
    }

    #[test]
    fn live中の英語分割はbackspaceで読みを1文字ずつ戻す() {
        let (mut c, _) = live_split_composer();
        type_keys(&mut c, "artifactwoko");
        assert!(c.backspace());
        assert_eq!(c.preedit(), "artifactを");
    }

    #[test]
    fn live中の英語分割を表示している間は候補を選んでも確定も学習もしない() {
        let (mut c, log) = live_split_composer();
        type_keys(&mut c, "artifactwoko");
        assert!(c.candidates().is_empty());
        assert!(!c.select_candidate(0));
        assert!(log.selects.borrow().is_empty());
    }

    #[test]
    fn live中でも英語分割に戻れない末尾の子音はrawのまま確定する() {
        let (mut c, _) = live_split_composer();
        type_keys(&mut c, "artifactwok");
        c.flush_romaji();
        assert_eq!(c.phase(), Phase::Dangling);
        assert_eq!(commit_learning(&mut c), "artifactwok");
    }

    #[test]
    fn スペース変換の英語分割は変換せずに英単語とかなを表示する() {
        let (mut c, log) = live_split_composer();
        c.set_live(false);
        type_keys(&mut c, "artifactwo");
        assert_eq!(c.preedit(), "artifactを");
        assert!(log.converts.borrow().iter().all(|(_, reading)| reading != "を"));
    }

    #[test]
    fn mixedの変換はかな混じりが第1候補でrawがその直後に並ぶ() {
        let (mut c, log) = composer();
        c.push_input("i", "い", "");
        c.push_literal("Phone");
        assert!(c.begin_conversion());
        assert_eq!(texts(&c), vec!["いPhone", "iPhone"]);
        assert_eq!(commit_learning(&mut c), "いPhone");
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

    fn loan_composer(reading: &str, candidates: &[&str]) -> Composer<MockEngine, FakeLexicon> {
        let engine = MockEngine {
            log: Log::default(),
            table: vec![(
                reading.to_string(),
                candidates.iter().map(|s| s.to_string()).collect(),
            )],
        };
        Composer::new(engine, FakeLexicon::default().with_english(loanwords))
    }

    #[test]
    fn カタカナ語は英単語が第1候補の直後に入り英単語でないrawは末尾() {
        let mut c = loan_composer("ですくとっぷ", &["デスクトップ", "デス区トップ"]);
        c.push_input("desukutoppu", "ですくとっぷ", "");
        c.begin_conversion();
        assert_eq!(
            texts(&c),
            vec!["デスクトップ", "desktop", "デス区トップ", "desukutoppu"]
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
        assert_eq!(commit_learning(&mut c), "desktop");
        assert!(c.engine.log.selects.borrow().is_empty());
    }

    #[test]
    fn 小書きのかなを含む語もrawとカタカナ語の候補を持つ() {
        let engine = MockEngine {
            log: Log::default(),
            table: vec![("うぃんど".to_string(), vec!["ウィンド".to_string()])],
        };
        let mut c = Composer::new(engine, FakeLexicon::default().with_english(|k| {
            if k == "ウィンド" { vec!["wind".to_string()] } else { Vec::new() }
        }));
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

    fn emojis(reading: &str) -> Vec<String> {
        match reading {
            "ねこ" => ["😺", "😸", "😹", "😻", "😼", "🐱", "🐈"].map(String::from).to_vec(),
            "かんぱい" => vec!["🍻".to_string(), "X1".to_string()],
            "いぬ" => vec!["🐶".to_string()],
            _ => Vec::new(),
        }
    }

    fn emoji_composer(reading: &str, candidates: &[&str]) -> Composer<MockEngine, FakeLexicon> {
        let engine = MockEngine {
            log: Log::default(),
            table: vec![(
                reading.to_string(),
                candidates.iter().map(|s| s.to_string()).collect(),
            )],
        };
        Composer::new(engine, FakeLexicon::default().with_emoji(emojis))
    }

    #[test]
    fn 絵文字は上位3件のエンジン候補の後ろに入り最大5件で末尾のrawを押し出さない() {
        let mut c = emoji_composer("ねこ", &["猫", "ネコ", "根子", "寝こ", "値こ"]);
        c.push_input("neko", "ねこ", "");
        c.begin_conversion();
        assert_eq!(
            texts(&c),
            vec!["猫", "ネコ", "根子", "😺", "😸", "😹", "😻", "😼", "寝こ", "値こ", "neko"]
        );
        assert_eq!(c.candidates()[3].engine_rank(), None);
    }

    #[test]
    fn 絵文字は3番目より前には入らない() {
        let mut c = emoji_composer("いぬ", &["犬"]);
        c.push_input("inu", "いぬ", "");
        c.begin_conversion();
        assert_eq!(texts(&c), vec!["犬", "🐶", "inu"]);
        let mut c = emoji_composer("いぬ", &[]);
        c.push_input("inu", "いぬ", "");
        c.begin_conversion();
        assert_eq!(texts(&c), vec!["いぬ", "イヌ", "🐶", "inu"]);
    }

    #[test]
    fn 絵文字を選んで確定するとその文字列が出て学習しない() {
        let mut c = emoji_composer("かんぱい", &["乾杯", "完敗", "缶杯"]);
        c.push_input("kanpai", "かんぱい", "");
        c.begin_conversion();
        let index = texts(&c).iter().position(|t| *t == "🍻").unwrap();
        assert!(c.select_candidate(index));
        assert_eq!(commit_learning(&mut c), "🍻");
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
        let mut c = Composer::new(engine, FakeLexicon::default().with_emoji(|_| vec!["😺".to_string()]));
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

    fn typed_romaji(keys: &str) -> Composer<MockEngine, FakeLexicon> {
        let (mut c, _) = composer();
        for ch in keys.chars() {
            c.type_romaji(&ch.to_string());
        }
        c
    }

    #[test]
    fn 未確定のローマ字は_preedit_に出るが_commit_preview_には入らない() {
        let c = typed_romaji("kyout");
        assert!(c.has_romaji());
        assert_eq!(c.preedit(), "きょうt");
        assert_eq!(c.commit_preview(), "きょう");
    }

    #[test]
    fn 未確定のローマ字は_backspace_と_clear_で消える() {
        let mut c = typed_romaji("kyout");
        c.backspace_romaji();
        assert!(!c.has_romaji());
        assert_eq!(c.preedit(), "きょう");
        let mut c = typed_romaji("kyout");
        c.clear_romaji();
        assert_eq!(c.preedit(), "きょう");
    }

    #[test]
    fn tt_の_2_つ目の_t_は確定した単位に含めず持ち越す() {
        let mut c = typed_romaji("katta");
        assert_eq!(c.raw(), "katta");
        assert_eq!(c.preedit(), "かった");
        c.backspace();
        assert_eq!(c.raw(), "kat");
        assert_eq!(c.preedit(), "かっ");
    }

    #[test]
    fn 英語と見なす入力中は_preedit_が打鍵のままで_commit_preview_も同じになる() {
        let c = typed_romaji("merge");
        assert!(!c.has_romaji());
        assert_eq!(c.phase(), Phase::Dangling);
        assert_eq!(c.display(), "めrげ");
        assert_eq!(c.preedit(), "merge");
        assert_eq!(c.commit_preview(), "merge");
    }

    #[test]
    fn commit_preview_は状態を変えず_commit_と同じ文字列を返す() {
        for keys in ["kyou", "merge", "kyouh", "entery"] {
            let mut c = typed_romaji(keys);
            c.flush_romaji();
            let preview = c.commit_preview();
            assert_eq!(c.commit().text, preview, "{keys}");
        }
    }

    /// 英単語 issue / made を辞書の英単語とし、`headwords` を辞書の見出し、`prefers` を審判の答えにしたもの。
    fn word_composer(headwords: &[&str], prefers: bool) -> (Composer<MockEngine, FakeLexicon>, Log) {
        let log = Log::default();
        *log.headwords.borrow_mut() = headwords.iter().map(ToString::to_string).collect();
        *log.prefers.borrow_mut() = prefers;
        let engine = MockEngine { log: log.clone(), table: vec![] };
        let lexicon = FakeLexicon::default().with_english_word(|w| matches!(w, "issue" | "made"));
        (Composer::new(engine, lexicon), log)
    }

    #[test]
    fn かなの読みが見出しでない英単語は審判なしで英語として打つ() {
        let (mut c, log) = word_composer(&[], false);
        type_keys(&mut c, "issue");
        assert_eq!(c.phase(), Phase::Dangling);
        assert_eq!(c.preedit(), "issue");
        assert_eq!(c.commit_preview(), "issue");
        assert!(log.asked.borrow().is_empty());
        let committed = c.commit();
        assert_eq!(committed.text, "issue");
        assert_eq!(committed.english, vec!["issue".to_string()]);
    }

    #[test]
    fn かなの読みが見出しでない英単語はliveでも英字のまま() {
        let (mut c, _) = word_composer(&[], false);
        c.set_live(true);
        type_keys(&mut c, "issue");
        assert_eq!(c.preedit(), "issue");
        assert_eq!(c.commit().text, "issue");
    }

    #[test]
    fn 見出しでない英単語は候補を求めてもrawが先頭になる() {
        let (mut c, _) = word_composer(&[], false);
        type_keys(&mut c, "issue");
        assert!(c.begin_conversion());
        assert_eq!(c.display(), "issue");
    }

    #[test]
    fn かなの読みが見出しの英単語は審判が英語を選べば英語になる() {
        let (mut c, log) = word_composer(&["まで"], true);
        type_keys(&mut c, "made");
        assert_eq!(c.phase(), Phase::Dangling);
        assert_eq!(c.preedit(), "made");
        assert_eq!(c.commit_preview(), "made");
        assert_eq!(*log.asked.borrow(), vec![("made".to_string(), "までK1".to_string())]);
    }

    #[test]
    fn 審判が英語を選ばなければかなのまま() {
        let (mut c, log) = word_composer(&["まで"], false);
        c.set_live(true);
        type_keys(&mut c, "made");
        assert_eq!(c.phase(), Phase::Composing);
        assert_eq!(c.preedit(), "まで");
        assert_eq!(log.asked.borrow().len(), 1);
    }

    #[test]
    fn 審判が決めた英語の確定は学習しない() {
        let (mut c, _) = word_composer(&["まで"], true);
        type_keys(&mut c, "made");
        let committed = c.commit();
        assert_eq!(committed.text, "made");
        assert!(committed.english.is_empty());
    }

    #[test]
    fn 見出しでない英単語に審判は使わない() {
        let (mut c, log) = word_composer(&["まで"], true);
        type_keys(&mut c, "issue");
        assert_eq!(c.phase(), Phase::Dangling);
        assert!(log.asked.borrow().is_empty());
    }

    #[test]
    fn 英単語のあとに打鍵が続いて語でなくなればかなに戻る() {
        for (headwords, prefers) in [(&[][..], false), (&["いっすえ"][..], true)] {
            let (mut c, _) = word_composer(headwords, prefers);
            type_keys(&mut c, "issue");
            assert_eq!(c.phase(), Phase::Dangling);
            c.type_romaji("n");
            assert_eq!(c.phase(), Phase::Composing);
            assert_eq!(c.preedit(), "いっすえn");
            c.backspace_romaji();
            assert_eq!(c.phase(), Phase::Dangling, "{headwords:?}");
            assert_eq!(c.preedit(), "issue");
        }
    }

    #[test]
    fn 英語のまま読みを1文字消すと同じ打鍵を直接打ったときと同じ表示になる() {
        for (headwords, prefers, live) in [(&[][..], false, false), (&["いっすえ"][..], true, false), (&[][..], false, true)] {
            let (mut c, _) = word_composer(headwords, prefers);
            c.set_live(live);
            type_keys(&mut c, "issue");
            assert_eq!(c.phase(), Phase::Dangling);
            c.backspace();
            let (mut direct, _) = word_composer(headwords, prefers);
            direct.set_live(live);
            type_keys(&mut direct, "issu");
            assert_eq!(c.raw(), "issu");
            assert_eq!(c.phase(), direct.phase(), "{headwords:?} {live}");
            assert_eq!(c.preedit(), direct.preedit(), "{headwords:?} {live}");
            assert_eq!(c.commit_preview(), direct.commit_preview(), "{headwords:?} {live}");
        }
    }

    #[test]
    fn 審判が読みをはっきり選べば見出しでない英単語もかなのまま() {
        let (mut c, log) = word_composer(&[], false);
        *log.rejects.borrow_mut() = true;
        type_keys(&mut c, "issue");
        assert_eq!(c.phase(), Phase::Composing);
        assert_eq!(c.preedit(), "いっすえ");
        assert!(log.asked.borrow().is_empty());
    }

    #[test]
    fn 辞書を開けていない間は見出しでない英単語も英語にしない() {
        let (mut c, log) = word_composer(&[], false);
        *log.no_dictionary.borrow_mut() = true;
        type_keys(&mut c, "issue");
        assert_eq!(c.phase(), Phase::Composing);
    }

    #[test]
    fn 固定中に消して英単語に戻ると直接打ったときと同じ英語になる() {
        let (mut c, _) = word_composer(&[], false);
        c.set_live(true);
        type_keys(&mut c, "issuea");
        c.backspace();
        let (mut direct, _) = word_composer(&[], false);
        direct.set_live(true);
        type_keys(&mut direct, "issue");
        assert_eq!(c.phase(), direct.phase());
        assert_eq!(c.preedit(), direct.preedit());
        assert_eq!(c.commit_preview(), "issue");
    }
}
