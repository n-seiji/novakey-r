use super::*;
use crate::deps::{EnglishMemory, FakeLexicon};
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
    c.push_flush(Some("きょうh".into()));
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
    c.push_flush(Some("r".into()));
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
    c.push_flush(Some("きょうh".into()));
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
fn エンジン候補の後ろに同じ英単語があれば複製を消して決まった位置に入れ_他の候補の添字は変わらない() {
    let mut c = loan_composer("ですくとっぷ", &["デスクトップ", "デス区トップ", "desktop", "デスク取っぷ"]);
    c.push_input("desukutoppu", "ですくとっぷ", "");
    c.begin_conversion();
    let t = texts(&c);
    assert_eq!(&t[..4], ["デスクトップ", "desktop", "デス区トップ", "デスク取っぷ"]);
    assert_eq!(t.iter().filter(|x| **x == "desktop").count(), 1);
    assert_eq!(c.candidates()[1].engine_rank(), None);
    assert_eq!(c.candidates()[2].engine_rank(), Some(EngineRank(1)));
    assert_eq!(c.candidates()[3].engine_rank(), Some(EngineRank(3)));
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
    c.push_flush(Some("r".into()));
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
    c.push_flush(Some("r".into()));
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

/// 英単語 done / the / made / to (辞書の語として) と `headwords` を持ち、覚えた語を見られる Composer。
fn learn_composer(headwords: &[&str]) -> (Composer<MockEngine, FakeLexicon>, Log, crate::deps::FakeMemory) {
    let log = Log::default();
    *log.headwords.borrow_mut() = headwords.iter().map(ToString::to_string).collect();
    let engine = MockEngine { log: log.clone(), table: vec![] };
    let lexicon = FakeLexicon::default().with_english_word(|w| matches!(w, "done" | "the" | "made" | "to"));
    let memory = lexicon.memory();
    (Composer::new(engine, lexicon), log, memory)
}

/// raw を打って変換を始め、候補の text を選んだ状態にする。
fn pick(c: &mut Composer<MockEngine, FakeLexicon>, raw: &str, text: &str) {
    type_keys(c, raw);
    assert!(c.begin_conversion());
    assert!(c.select_candidate_text(text), "{text} is not a candidate");
}

#[test]
fn 候補から選んだ英字そのものは英語として選んだことになる() {
    for raw in ["done", "the"] {
        let (mut c, _, _) = learn_composer(&["どね", "てぇ"]);
        pick(&mut c, raw, raw);
        let committed = c.commit();
        assert_eq!(committed.text, raw);
        assert_eq!(committed.picked.as_deref(), Some(raw));
        assert!(committed.english.is_empty());
    }
}

#[test]
fn 候補から選んだのが英字そのものでなければ英語として選んだことにならない() {
    let (mut c, _, _) = learn_composer(&["どね"]);
    pick(&mut c, "done", "どねK1");
    assert_eq!(c.commit().picked, None);
    // 読みのまま確定 (Enter) も選んだことにならない
    let (mut c, _, _) = learn_composer(&["どね"]);
    type_keys(&mut c, "done");
    assert_eq!(c.commit().picked, None);
}

#[test]
fn 英単語でない読みの打ちかけは英語として選んでも覚えない() {
    // donewo や kazu は辞書の英単語ではないので、その raw 候補を選んでも覚えない。to は辞書の語でも 3 文字未満なので対象外。
    let (mut c, _, _) = learn_composer(&[]);
    pick(&mut c, "donewo", "donewo");
    assert_eq!(c.commit().picked, None);
    let (mut c, _, _) = learn_composer(&[]);
    pick(&mut c, "to", "to");
    assert_eq!(c.commit().picked, None);
    let (mut c, _, _) = learn_composer(&[]);
    pick(&mut c, "kazu", "kazu");
    assert_eq!(c.commit().picked, None);
}

#[test]
fn 審判が英語と決めた語は候補から選び直しても覚えない() {
    let (mut c, _, _) = learn_composer(&["まで"]);
    *c.engine.log.prefers.borrow_mut() = true;
    type_keys(&mut c, "made");
    assert_eq!(c.phase(), Phase::Dangling);
    assert!(c.begin_conversion());
    assert!(c.select_candidate_text("made"));
    assert_eq!(c.commit().picked, None);
}

#[test]
fn 英単語と助詞に分けたかな混じりは英語として選んだことにならない() {
    // かな混じりの確定文字列は raw と一致しないので、text == raw の条件だけで学習の対象から外れる。
    let (mut c, _, _) = learn_composer(&[]);
    type_keys(&mut c, "doneni");
    assert!(c.begin_conversion());
    assert_eq!(c.commit().picked, None);
}

#[test]
fn 英語として選んだ語は見出しの読みがあっても以後英語で打つ() {
    let (mut c, log, mut memory) = learn_composer(&["どね"]);
    memory.learn("done");
    type_keys(&mut c, "done");
    assert_eq!(c.phase(), Phase::Dangling);
    assert_eq!(c.preedit(), "done");
    assert_eq!(c.commit_preview(), "done");
    assert!(log.asked.borrow().is_empty());
}

#[test]
fn 英語として選んだ語でも審判が却下すれば読みのまま() {
    let (mut c, log, mut memory) = learn_composer(&["どね"]);
    memory.learn("done");
    *log.rejects.borrow_mut() = true;
    type_keys(&mut c, "done");
    assert_eq!(c.phase(), Phase::Composing);
    assert_eq!(c.preedit(), "どね");
}

#[test]
fn 英語として選んだことのない見出しの語はゲートと審判のまま() {
    let (mut c, _, _) = learn_composer(&["どね"]);
    type_keys(&mut c, "done");
    assert_eq!(c.phase(), Phase::Composing);
    assert_eq!(c.preedit(), "どね");
}

#[test]
fn 英語として選んだ語の続きに読みを打つと分割しない() {
    // 見出しのある語は、覚えていても donewo を done + を に切らない。
    let (mut c, _, mut memory) = learn_composer(&["どね"]);
    memory.learn("done");
    type_keys(&mut c, "donewo");
    assert_eq!(c.english_split(true), None);
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
