#![forbid(unsafe_code)]

// composer.rs - 未確定文の状態機械: 入力単位(ローマ字+かな) + 同期変換候補 + フェーズ。
// 候補は {text, source} で持ち、リランキング後も学習には Source::Engine の元の順位を渡す。
// ローマ字そのまま(raw)など engine 以外の候補は順位を持たず、確定しても学習しない。

use std::borrow::Cow;

use crate::deps::Lexicon;
use crate::english_split;
use crate::kkc::live_backspace;
use crate::romaji_converter::RomajiConverter;
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
    /// 候補一覧から英字そのもの (done, the) を選んで確定した語。以後その語を英語で打つ合図 (`Composer::picked_english`)。
    /// `english` と分けるのは、commitComposition (フォーカス喪失など) では「選んだ」とは見なさず学習しないため。
    pub picked: Option<String>,
}

pub trait CandidateEngine {
    fn convert(&mut self, revision: u64, hiragana: &str) -> Vec<String>;
    fn select(&mut self, revision: u64, engine_index: usize) -> bool;
    fn cancel(&mut self);
    /// 変換の文脈 (アプリで直前に確定した文字列。空なら文脈なし) と、そのアプリが英語を打つアプリか。キーごとに、変換する前に渡される。
    fn set_context(&mut self, _context: &str, _english_app: bool) {}
    /// ひらがなの読みがそのまま辞書の見出しにあるか (made → まで)。英単語として切り分ける前の衝突確認に使う。
    fn has_reading(&self, _hiragana: &str) -> bool {
        false
    }
    /// `text` の方が `over` より、いまの文脈で文として自然か。審判の差が余裕を超えたときだけ true。審判が使えない engine は false。
    fn prefers_text(&mut self, _text: &str, _over: &str) -> bool {
        false
    }
    /// ひらがなの読みで始まる辞書の見出しがあるか (あぴー → アピール)。英単語の直後の `-` が長音として続くかの確認に使う。
    fn has_reading_prefix(&self, _hiragana: &str) -> bool {
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
    /// かなの読みが辞書の見出しでもある語を、審判が英語と決めた。学習しない (Judged の語を覚えると、審判の推測が規則 1 の英語として固定されてしまうため)。
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
    /// カーソルが 1 ページ目 (数字キーで選べる先頭 9 件) の外へ出たら true。パネルがスクロールして行の見え方が分からなくなるので、
    /// 数字キーで選ばない。先頭 (#1) に戻ればパネルも最上段なので false に戻る。
    scrolled: bool,
}

/// 数字キーで選べる先頭の件数。
const DIGIT_PICKS: usize = 9;

impl Selection {
    /// カーソルを index へ動かし、候補ウィンドウを出す状態にする。scrolled の更新はここだけ。
    fn move_to(&mut self, index: usize) {
        self.cursor = index;
        self.window = true;
        self.scrolled = index != 0 && (self.scrolled || index >= DIGIT_PICKS);
    }
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

    /// 子音 1 打鍵だけから生まれたかな (kk の っ、nk の ん)。母音を打っていないので、かなにする意図があったとは言えない。
    fn is_bare_consonant(&self) -> bool {
        match self {
            Segment::Kana { keys, .. } => matches!(keys.as_bytes(), [b] if b.is_ascii_lowercase() && !b"aiueo".contains(b)),
            Segment::Literal(_) => false,
        }
    }

    /// 英語分割の英単語 (Literal) とかな部分 (Kana) を、区切りの並びにする。
    fn from_split(head: String, tail: Vec<(String, String)>) -> impl Iterator<Item = Segment> {
        std::iter::once(Segment::Literal(head)).chain(tail.into_iter().map(|(keys, kana)| Segment::Kana { keys, kana }))
    }
}

/// 1 回の変換で入れる絵文字の上限
const EMOJI_LIMIT: usize = 5;
/// 絵文字より前に並べるエンジン候補の数
const EMOJI_AFTER_ENGINE: usize = 3;
/// live 変換は、読みがこの文字数 (かな) を超えてから表示を変換する。これ以下はひらがなのまま (Space の変換は別)。
const LIVE_MIN_KANA: usize = 2;
/// 未確定のローマ字がこれだけのとき、読みには `PENDING_N_KANA` を足して見せる (`Composer::pending_kana`)。
const PENDING_N: &str = "n";
const PENDING_N_KANA: &str = "ん";

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
                    let converted = self.converted_first();
                    Cow::Owned(pin.surface() + converted.map_or(self.rest(), |c| c.text.as_str()))
                }
            },
            Stage::Editing if self.typing == Typing::Mixed && self.live => {
                let converted = self.converted_first();
                Cow::Borrowed(converted.map_or(self.hiragana.as_str(), |c| c.text.as_str()))
            }
            Stage::Transliterated { text, .. } => Cow::Borrowed(text),
            _ => Cow::Borrowed(&self.hiragana),
        }
    }

    /// 下線付きで見せる文字列。表示の決定はここだけ。
    pub fn preedit(&self) -> String {
        if self.is_whole_word_pending() {
            return self.raw();
        }
        if let Some(split) = self.shown_split() {
            // 未確定の n だけは、Enter の flush で ん になって分割の読みに入る (commit_preview と同じ分割)。
            let split = if split.pending == PENDING_N { self.english_split(true).unwrap_or(split) } else { split };
            return match self.live_split_text(&split) {
                Some(text) => text.to_string() + &split.pending,
                None => split.display(),
            };
        }
        if (self.typing == Typing::English && self.is_kana_editing()) || self.is_typing_english() {
            return self.raw();
        }
        self.kana_view()
    }

    /// 英語として扱わないときの、見せる文字列と Enter で確定する文字列 (どちらもここから出す)。
    /// 読み (display) に、未確定のローマ字を足したもの。n だけなら読みが ん として含んでいる (`pending_kana`)。
    /// ただし、かなが 1 つも無く子音だけ (k, nk, kk) なら、かなにする意図が無いので打鍵のまま見せる。
    fn kana_view(&self) -> String {
        if self.has_romaji() && self.segments.iter().all(Segment::is_bare_consonant) {
            return self.raw();
        }
        let pending = if self.pending_kana().is_empty() { self.romaji.buffer() } else { "" };
        format!("{}{pending}", self.display())
    }

    /// Enter で確定される文字列 (状態は変えない)。表示 (`preedit`) と同じ判定で、未確定ローマ字は表示のまま足す。
    /// かな入力中は、確定の前に行う flush が局面を英語に変える分 (merge, artifactni) も見込む。
    pub fn commit_preview(&self) -> String {
        match self.settle(self.is_typing_english()) {
            Settle::Mixed(split) => match self.live_split_text(&split) {
                Some(text) => text.to_string(),
                None => format!("{}{}", split.head, split.tail_kana()),
            },
            Settle::Dangling => self.raw(),
            Settle::Unchanged if self.phase() == Phase::Dangling => self.raw(),
            Settle::Unchanged => self.kana_view(),
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

    /// 読みに足して見せる、未確定のローマ字のかな。n だけなら ん (次の打鍵で `mina` の な や `ny` に差し替わる)、それ以外は空。
    /// かなを入力中 (Typing::Kana の Editing) で、Enter の flush が組成を英語として扱い直さない (raw で確定しない) ときだけ。
    /// 扱い直す組成 (ASCII のまま残ったかながある) は、n も打鍵のまま見せる。英語分割は読みに ん を足す側で、`preedit` が分割ごと作り直す。この条件を決めるのはここだけ。
    fn pending_kana(&self) -> &'static str {
        let folded = matches!(self.stage, Stage::Editing)
            && self.typing == Typing::Kana
            && !self.segments.is_empty()
            && self.romaji.buffer() == PENDING_N
            && !self.segments.iter().all(Segment::is_bare_consonant)
            && matches!(self.settle(self.is_typing_english()), Settle::Unchanged);
        if folded { PENDING_N_KANA } else { "" }
    }

    /// 見えている読み: 確定した区切りのかなに、未確定の n を ん として足したもの。
    fn shown_reading(&self) -> String {
        self.segments.iter().map(Segment::reading).chain(std::iter::once(self.pending_kana())).collect()
    }

    fn sync_hiragana(&mut self) {
        self.hiragana = self.shown_reading();
    }

    /// 未確定の n を ん として見せるかの変化を、読みと候補に反映する。読みが変わったときだけ、新しい世代で変換し直す。
    /// 英語を打っている間は変換しないので、読みの ん を外すだけ。
    fn sync_pending_kana(&mut self) {
        if self.shown_reading() == self.hiragana {
            return;
        }
        if matches!(self.stage, Stage::Editing) && self.typing == Typing::Kana {
            self.refresh();
        } else {
            self.sync_hiragana();
        }
    }

    /// 未確定のローマ字が変わった打鍵のあとの後始末。英語かの判定を直してから、n を ん として見せるかを読みに反映する。
    fn settle_pending(&mut self) {
        self.track_english();
        self.sync_pending_kana();
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
        self.stage = Stage::Editing;
        // 読みの ん (`pending_kana`) は Kana の入力中だけ足すので、読みを作る前に局面を Kana にしておく。
        self.typing = Typing::Kana;
        self.sync_hiragana();
        if self.hiragana.is_empty() {
            self.pin = None;
            self.typing = Typing::Empty;
            return;
        }
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
        self.sync_hiragana();
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

    /// 読みの候補列 (`reading.list`) の第 1 候補が engine の変換結果ならそれ。記号の異体などは None。
    fn converted_first(&self) -> Option<&Candidate> {
        self.reading.list.first().filter(|c| c.engine_rank().is_some())
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
        let rest = self.rest().to_string();
        self.convert_mixed("", &rest)
    }

    /// 読みの `->` を `>` としてエンジンに渡して変換し、返った候補の同じ位置の `>` を `->` に戻す。
    /// エンジンに `-` を見せないので、学習や順位が矢印のために崩れない。
    fn convert_with_arrows(&mut self, reading: &str) -> Vec<String> {
        if !reading.contains("->") {
            return self.engine.convert(self.reading.generation, reading);
        }
        let plain = reading.replace("->", ">");
        // 読みの中で、何番目の `>` が矢印だったか。
        let mut arrow_slots = Vec::new();
        let mut rest = reading;
        while let Some(at) = rest.find('>') {
            arrow_slots.push(at > 0 && rest.as_bytes()[at - 1] == b'-');
            rest = &rest[at + 1..];
        }
        let mut texts = self.engine.convert(self.reading.generation, &plain);
        for text in &mut texts {
            if text.matches('>').count() != arrow_slots.len() {
                continue;
            }
            let mut index = 0;
            *text = text
                .chars()
                .map(|ch| {
                    if ch != '>' {
                        return ch.to_string();
                    }
                    index += 1;
                    if arrow_slots[index - 1] { "->".to_string() } else { ">".to_string() }
                })
                .collect();
        }
        texts
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

    /// 未確定のローマ字を確定かなとして取り込む。変換しきれない子音が残れば (`enter` の `r`) 局面を Dangling にする。
    pub fn flush_romaji(&mut self) {
        let english = self.is_typing_english();
        let keys = self.romaji.buffer().to_string();
        let flush = self.romaji.flush();
        self.absorb_flush(english, keys, flush);
    }

    /// 確定のための flush。表示がかな + 未確定のローマ字のままなら (kyout)、flush しない:
    /// 子音を ASCII のかなとして取り込むと英語の合図 (`has_ascii_kana`) になり、表示と違う raw で確定してしまう。
    /// その場合は `commit_preview` が未確定のローマ字を表示のまま足す。英語として扱い直す組成だけ flush する。
    /// かなが 1 つも無い (子音だけの k, n) ときは、打鍵のまま ASCII のかなとして取り込み (n を ん にしない)、raw で確定する。
    pub fn flush_romaji_for_commit(&mut self) {
        if self.has_romaji() && self.segments.is_empty() {
            let english = self.is_typing_english();
            let keys = self.romaji.buffer().to_string();
            self.romaji.clear();
            self.absorb_flush(english, keys.clone(), Some(keys));
            return;
        }
        let shown_as_kana = self.has_romaji()
            && matches!(self.stage, Stage::Editing)
            && self.typing == Typing::Kana
            && matches!(self.settle(self.is_typing_english()), Settle::Unchanged);
        if !shown_as_kana {
            self.flush_romaji();
        }
    }

    /// english は、未確定のローマ字を残した状態で英語と見なせていたか。
    fn absorb_flush(&mut self, english: bool, keys: String, flush: Option<String>) {
        if let Some(kana) = flush {
            self.push_kana(keys, &kana);
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
        // 打鍵列全体が英単語なら、語の途中で切れる分割 (refactor の refac + と + r) より語そのものを優先する。
        if english && self.is_whole_word_typed() {
            return Settle::Dangling;
        }
        // 見えている分割に未確定のローマ字が付いている間 (artifactwok の artifactをk) は、その表示のまま確定する。
        // n だけは対象外: 確定で ん になり、分割の読みに入る (kousin の n)。
        if let Some(split) = self.english_split(false).filter(|s| !s.pending.is_empty() && s.pending != PENDING_N) {
            return Settle::Mixed(split.pending_as_kana());
        }
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
        if self.is_english_charset(&raw) && self.is_whole_word(&raw) {
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

    /// 英単語 head のあとに、かな tail をエンジンで変換したものを付けた候補列。head が空なら tail の変換結果だけ。
    fn convert_mixed(&mut self, head: &str, tail: &str) -> Vec<Candidate> {
        let mut list: Vec<Candidate> = Vec::new();
        for (engine_index, text) in self.convert_with_arrows(tail).into_iter().enumerate() {
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
        self.segments = Segment::from_split(split.head, split.tail).collect();
        self.romaji.clear();
        self.sync_hiragana();
        self.reset_candidates();
        self.stage = Stage::Editing;
        self.typing = Typing::Mixed;
        self.split_origin = Some(origin);
        self.reading.list = self.convert_mixed(&head_text, &tail);
    }

    /// 英語を打っていると見なせる局面。英字のまま表示し、Enter の確定もその英字にする (表示と確定は同じ判定を共有する)。
    /// かなの途中に変換できない子音が挟まった (merge の めrげ) か、かなにしきれない打鍵が残り、
    /// 打鍵列全体が英語として確定したことのある語・開発用語の一覧の語 (git) か 4 文字以上の英単語 (enter) になっている。
    /// 一覧の語は数字を含んでよい (base64)。打鍵列全体がその語になったときだけで、手前 (base6) や一覧にない数字付き (kyou1) は対象外 (`is_english_charset`)。
    /// 先頭が `/` でも、残りがその語なら同じ (/posts)。表の短い語は日本語の打ちかけ (in, den) と紛れるので外す。
    /// 語でない打鍵列に子音が残っただけ (kyout) は英語とせず、かな表示のまま確定する (`kana_view`)。
    pub fn is_typing_english(&self) -> bool {
        let raw = self.raw();
        let typed = raw.strip_prefix('/').unwrap_or(&raw);
        let word = || self.is_whole_word_pending();
        self.is_kana_editing()
            && self.is_english_charset(typed)
            && (self.has_ascii_kana() || word() || self.is_verdict(&raw, Verdict::Word) || self.is_verdict(&raw, Verdict::Judged))
    }

    /// 打鍵列全体が、未確定のローマ字もかなにしきれない文字も残らない英単語か。
    fn is_whole_word(&self, raw: &str) -> bool {
        self.romaji.buffer().is_empty() && !self.has_ascii_kana() && self.is_english_word_raw(raw)
    }

    /// 組成全体が英単語なら、英語として打つかを判定して持つ。そうでなければ持たない。
    /// かなの読みが辞書の見出しでない語 (issue → いっすえ) は、審判が読みの方をはっきり選ばない限り英語 (審判がなくても英語)。
    /// 見出しでもある語 (made, home) は、審判が英語を選んだときだけ英語。比べる相手は engine の第 1 候補。
    /// ただし、候補一覧から選んで英語として確定したことのある語 (done → どね) は見出しでも前者と同じ扱い: 審判が読みをはっきり選ばない限り英語。
    /// 審判がまだ読み込めていない間は却下しないので、そのときは必ず英語になる。
    /// 開発用語の一覧 (dev_words.txt) の語は、見出しかどうかにも審判の却下にもよらず必ず英語 (api → あぴ にしない)。審判は小さいモデルで、api や repo のような語を読みの方に寄せてしまうため。一覧の語は、読みが日本語の語になる語を入れない約束で選んでいる。覚えた語と表の語 (loanwords.tsv) は、上の判定のまま。
    /// 固定中は表示と比べ、英語になるときだけ固定が外れる (`enter_english`)。raw が変わりうる所 (refresh / track_english / unpin) で呼ぶ。
    fn judge_word(&mut self) {
        let raw = self.raw();
        let eligible = self.is_kana_editing()
            && self.is_english_charset(&raw)
            && self.is_whole_word(&raw)
            && self.engine.has_dictionary();
        if !eligible {
            self.word_verdict = None;
            return;
        }
        // 比べる相手は engine の第 1 候補 (live の短い読みはひらがな表示でも同じ)。固定中は固定した分 + 残りの変換 (読みに戻すと見えている表示が変わる)。
        let converted = self.converted_first().map(|c| c.text.clone());
        let first = match &self.pin {
            Some(pin) => Some(pin.surface() + converted.as_deref().unwrap_or_else(|| self.rest())),
            None => converted,
        };
        let verdict = if self.lexicon.is_dev_word(&raw) {
            Verdict::Word
        } else if self.engine.has_reading(&self.hiragana) && !self.lexicon.knows_english(&raw) {
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

    /// Dangling の英単語の直後の `-` を、英単語の区切りではなく長音として読みに続けるか。
    /// 英語と判定された語のかな読み + ー で始まる見出しがあるときだけ (api-ru → アピール)。なければ英語のまま確定して `-` を入れる (repo-name, test-case)。
    /// 組成の途中の日本語に続く英単語 (kyouhaapi-) は対象外 (全体が 1 つの英単語のときだけ)。
    pub fn hyphen_continues_kana(&self) -> bool {
        let raw = self.raw();
        self.phase() == Phase::Dangling
            && self.pin.is_none()
            && self.is_verdict(&raw, Verdict::Word)
            && self.romaji.buffer().is_empty()
            && !self.hiragana.is_empty()
            && !self.has_ascii_kana()
            && self.engine.has_reading_prefix(&format!("{}ー", self.hiragana))
    }

    /// 組成全体 (raw) の判定が `verdict` か。
    fn is_verdict(&self, raw: &str, verdict: Verdict) -> bool {
        matches!(&self.word_verdict, Some((r, v)) if r == raw && *v == verdict)
    }

    /// かなにしきれない打鍵が残っていて、打鍵列全体 (先頭の `/` は除く) が英単語か。表示も Enter も、英語分割より先にこれで語そのものにする。
    fn is_whole_word_pending(&self) -> bool {
        !self.romaji.buffer().is_empty() && self.is_whole_word_typed()
    }

    /// 打鍵列全体 (先頭の `/` は除く) が英単語か。ローマ字が確定済みでもよい。
    fn is_whole_word_typed(&self) -> bool {
        let raw = self.raw();
        let typed = raw.strip_prefix('/').unwrap_or(&raw);
        self.is_kana_editing() && self.is_english_charset(typed) && self.is_english_word_raw(typed)
    }

    /// 英語を打っている途中 (Dangling) に数字を打つと、続けて開発用語の数字付きの語 (md5, utf8) か、その途中 (utf1) になるか。
    /// なるなら数字で確定せず組成に足す。ならない数字 (git1) は今までどおり確定して数字を入れる。
    pub fn digit_continues_dev_word(&self, digit: &str) -> bool {
        self.phase() == Phase::Dangling && self.pin.is_none() && self.romaji.buffer().is_empty() && self.is_english_charset(&(self.raw() + digit))
    }

    /// 打鍵列が英語の候補になりうる文字だけか。小文字の英字だけ、または開発用語の一覧の数字を含む語 (base64) かその途中 (base6)。
    /// 一覧の語に当たらない数字付きの打鍵列 (kyou1, git1) は今までどおり英語として扱わない。
    fn is_english_charset(&self, typed: &str) -> bool {
        let is_letter = |b: u8| b.is_ascii_lowercase();
        typed.bytes().all(is_letter)
            || (typed.bytes().next().is_some_and(is_letter)
                && typed.bytes().all(|b| is_letter(b) || b.is_ascii_digit())
                && self.lexicon.is_dev_word_prefix(typed))
    }

    /// 打鍵列全体が英語として確定したことのある語か、開発用語の一覧の語か、4 文字以上の英単語か。
    /// 表の短い語は日本語の打ちかけと紛れる (in, den) ので、4 文字未満は一覧の語 (git) だけ。
    fn is_english_word_raw(&self, raw: &str) -> bool {
        const MIN_LEN: usize = 4;
        self.lexicon.knows_english(raw)
            || self.lexicon.is_dev_word(raw)
            || (raw.len() >= MIN_LEN && self.lexicon.is_english_word(raw))
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
            self.settle_pending();
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
        self.settle_pending();
    }

    /// 未確定のローマ字を捨てる。
    pub fn clear_romaji(&mut self) {
        self.romaji.clear();
        self.sync_pending_kana();
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
            self.sync_pending_kana();
            return;
        }
        let split = pending.strip_suffix(carry).map_or(pending.len(), str::len);
        self.romaji.set_buffer(&pending[split..]);
        self.push_kana(pending[..split].to_string(), kana);
    }

    /// テスト用: 外の RomajiConverter が flush で返したかなを取り込む。
    #[cfg(test)]
    pub fn push_flush(&mut self, flush: Option<String>) {
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
            self.segments = Segment::from_split(split.head, split.tail)
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
        self.typing = self.literal_typing();
    }

    /// 英字入力を含む組成の打鍵の解釈。英字だけなら Literal、かなが混じれば Mixed。
    fn literal_typing(&self) -> Typing {
        if self.segments.iter().all(Segment::is_literal) {
            Typing::Literal
        } else {
            Typing::Mixed
        }
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
            // `->` の `>` を消したら、元の長音に戻る。
            Segment::Kana { keys, kana } if keys == "->" && kana == "->" => {
                *keys = "-".to_string();
                *kana = "ー".to_string();
                false
            }
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
            self.typing = self.literal_typing();
        } else {
            self.refresh();
        }
        true
    }

    /// 最初の変換。保留中のリランキングを反映し、raw 候補を第 1 候補の直後に入れて先頭を選ぶ。
    pub fn begin_conversion(&mut self) -> bool {
        self.unpin();
        let mut kept = None;
        if let Stage::Transliterated { kept: k, .. } = &mut self.stage {
            kept = k.take();
            self.stage = Stage::Editing;
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
        self.stage = Stage::Selecting(Selection { list, cursor: 0, window: false, scrolled: false });
        true
    }

    /// 次の候補へ進み(末尾なら先頭へ戻り)、候補ウィンドウを出す状態にする。
    pub fn advance(&mut self) -> bool {
        let Stage::Selecting(s) = &mut self.stage else {
            return false;
        };
        let next = (s.cursor + 1) % s.list.len();
        s.move_to(next);
        true
    }

    /// 前の候補へ戻り(先頭なら末尾へ回り)、候補ウィンドウを出す状態にする。
    pub fn retreat(&mut self) -> bool {
        let Stage::Selecting(s) = &mut self.stage else {
            return false;
        };
        let prev = (s.cursor + s.list.len() - 1) % s.list.len();
        s.move_to(prev);
        true
    }

    /// 数字キーが画面の行番号と一致するか。パネルが一度もスクロールしていない (カーソルが先頭 9 件の中だけにいた) ときだけ true。
    pub fn digits_pick(&self) -> bool {
        matches!(&self.stage, Stage::Selecting(s) if !s.scrolled)
    }

    pub fn select_candidate(&mut self, index: usize) -> bool {
        if index >= self.candidates().len() {
            return false;
        }
        if let Stage::Selecting(s) = &mut self.stage {
            s.move_to(index);
            return true;
        }
        let list = self.candidates().to_vec();
        let mut selection = Selection { list, cursor: 0, window: false, scrolled: false };
        selection.move_to(index);
        self.stage = Stage::Selecting(selection);
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
        self.clear();
    }

    /// 組成の状態を空に戻す。discard と commit の共通の後始末。
    fn clear(&mut self) {
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

    /// 候補一覧で選んでいる候補が、打鍵列そのままの英単語 (done, the) なら、その語。確定するとき「英語として選んだ」と見なして学習する。
    /// 英単語 (3 文字以上の辞書の語) だけに絞り、kazu や donewo のような読みの打ちかけは覚えない。
    /// かな混じり (Mixed) と、審判が決めた分 (Judged) は対象外 (Mixed は確定文字列が raw と一致しないので text == raw で外れる)。
    fn picked_english(&self) -> Option<String> {
        let Stage::Selecting(s) = &self.stage else {
            return None;
        };
        let raw = self.raw();
        let chosen = &s.list[s.cursor];
        let memorable = raw.len() >= 3 && raw.bytes().all(|b| b.is_ascii_lowercase()) && self.lexicon.is_english_word(&raw);
        (memorable && chosen.text == raw && chosen.engine_rank().is_none() && !self.is_verdict(&raw, Verdict::Judged)).then_some(raw)
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
        let picked = self.picked_english();
        // 固定した先頭部分を含む確定は、1 つの候補の順位では表せないので学習しない。
        let engine = match &self.stage {
            _ if self.pin.is_some() => None,
            Stage::Selecting(s) => s.list[s.cursor].engine_rank(),
            _ => self.live_rank(),
        }
        .map(|rank| (self.reading.generation, rank));
        self.clear();
        Committed { text, engine, english, picked }
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
        let position = list.len().min(1);
        let fresh: Vec<Candidate> = words
            .into_iter()
            .filter(|w| !Self::claim_slot(list, w, position))
            .map(|text| Candidate {
                text,
                source: Source::Loanword,
            })
            .collect();
        list.splice(position..position, fresh);
    }

    /// 組成全体が絵文字の読みと一致する場合、絵文字を最大 EMOJI_LIMIT 件入れる。
    fn insert_emoji_candidates(&self, list: &mut Vec<Candidate>, phase: Phase) {
        if matches!(phase, Phase::Dangling | Phase::Mixed) || self.is_symbol() {
            return;
        }
        let fresh: Vec<Candidate> = self.lexicon.emoji_for(&self.hiragana)
            .into_iter()
            .filter(|e| !Self::claim_slot(list, e, Self::emoji_position(list)))
            .take(EMOJI_LIMIT)
            .map(|text| Candidate {
                text,
                source: Source::Emoji,
            })
            .collect();
        // 絵文字は、上位 3 件のエンジン候補と英単語・raw の後ろに入れる (3 番目より前には入れない)。
        let position = Self::emoji_position(list);
        list.splice(position..position, fresh);
    }

    fn emoji_position(list: &[Candidate]) -> usize {
        let engine = list.iter().filter(|c| c.engine_rank().is_some()).count();
        let extras = list.len() - engine;
        (extras + engine.min(EMOJI_AFTER_ENGINE)).max(3).min(list.len())
    }

    /// `text` が `position` より前に既にあれば true (呼び出し側は足さない)。`position` 以降にエンジン候補として居るなら、その複製を消して
    /// false を返す (呼び出し側が決まった位置に入れる)。消す候補は後ろの重複なので、他の候補の `EngineRank` (学習の添字) は変わらない。
    fn claim_slot(list: &mut Vec<Candidate>, text: &str, position: usize) -> bool {
        if list.iter().take(position).any(|c| c.text == text) {
            return true;
        }
        match list.iter().skip(position).position(|c| c.text == text) {
            Some(i) if list[position + i].engine_rank().is_some() => {
                list.remove(position + i);
                false
            }
            Some(_) => true,
            None => false,
        }
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
mod tests;
