#![allow(unsafe_code)]

// simulate.rs - `--simulate '<keys>'`: 実際の打鍵経路 (input::route → Session) を headless で動かす。
// 変換は本物の azooKey、英単語は本物の loanwords.tsv と emoji.tsv。Cocoa には触れず、AI リランキングも使わない。
//
// キー表記: 通常の文字はそのまま。<space> <S-space> <enter> <esc> <bs> <tab> <up> <down> <left> <right>
// <C-k> (Ctrl+文字) <F6>..<F10> <lt> (`<`) <gt> (`>`)。

use std::path::PathBuf;

use crate::composer::{Composer, Phase};
use crate::input::{self, KeyInput, Modifiers, Route};
use crate::keymap::Key;
use crate::kkc::RustEngine;
use crate::session::{Effect, Session};
use crate::translit::Form;

/// 物理キー 1 打鍵。imk.rs の handleEvent が受け取るものと同じ粒度。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Event {
    key_code: u16,
    characters: String,
    shift: bool,
    control: bool,
}

impl Event {
    fn new(key_code: u16, characters: &str) -> Self {
        Self { key_code, characters: characters.to_string(), shift: false, control: false }
    }

    fn to_input(&self) -> KeyInput {
        KeyInput {
            key_code: self.key_code,
            characters: self.characters.clone(),
            // Ctrl 付きでも文字は変わらないので、そのまま「修飾キーを無視した文字」として使う。
            characters_ignoring_modifiers: self.characters.clone(),
            mods: Modifiers { shift: self.shift, control: self.control, command_or_option: false },
        }
    }
}

fn token_event(name: &str) -> Result<Event, String> {
    let ev = match name {
        "space" => Event::new(49, " "),
        "S-space" => Event { shift: true, ..Event::new(49, " ") },
        "enter" => Event::new(36, "\r"),
        "esc" => Event::new(53, "\u{1b}"),
        "bs" => Event::new(51, "\u{7f}"),
        "tab" => Event::new(48, "\t"),
        "up" => Event::new(126, "\u{F700}"),
        "down" => Event::new(125, "\u{F701}"),
        "left" => Event::new(123, "\u{F702}"),
        "right" => Event::new(124, "\u{F703}"),
        "lt" => Event::new(0, "<"),
        "gt" => Event::new(0, ">"),
        "F6" => Event::new(97, "\u{F709}"),
        "F7" => Event::new(98, "\u{F70A}"),
        "F8" => Event::new(100, "\u{F70B}"),
        "F9" => Event::new(101, "\u{F70C}"),
        "F10" => Event::new(109, "\u{F70D}"),
        _ => {
            let ctrl = name
                .strip_prefix("C-")
                .filter(|rest| rest.chars().count() == 1)
                .ok_or_else(|| format!("unknown key token <{name}>"))?;
            Event { control: true, ..Event::new(0, ctrl) }
        }
    };
    Ok(ev)
}

fn parse_keys(keys: &str) -> Result<Vec<(String, Event)>, String> {
    let mut events = Vec::new();
    let mut rest = keys;
    while let Some(ch) = rest.chars().next() {
        let named = (ch == '<')
            .then(|| rest.find('>').map(|end| &rest[1..end]))
            .flatten();
        if let Some(name) = named {
            events.push((format!("<{name}>"), token_event(name)?));
            rest = &rest[name.len() + 2..];
        } else {
            let mut ev = Event::new(0, &ch.to_string());
            ev.shift = ch.is_ascii_uppercase();
            events.push((ch.to_string(), ev));
            rest = &rest[ch.len_utf8()..];
        }
    }
    Ok(events)
}

/// 本物のアプリが持つ状態のうち、キー処理の結果として見えるもの。
struct Sim {
    session: Session<RustEngine>,
    /// アプリに確定済みの文字列。
    committed: String,
    /// アプリが表示している下線付きの未確定文字列 (最後の SetMarked)。
    marked: String,
    panel_visible: bool,
    panel_line: usize,
    /// アプリの下線付き文字列が session の持つ表示と食い違った打鍵 (ラベル)。
    stale: Vec<String>,
}

impl Sim {
    fn new() -> Self {
        let engine = RustEngine::new();
        let mut session = Session::new(Composer::new(engine)
                .with_english(crate::loanword::english_for)
                .with_english_word(crate::loanword::is_english_word)
                .with_english_memory(crate::english_memory::knows, crate::english_memory::learn)
                .with_emoji(crate::emoji::emoji_for));
        // 記号・数字の「遊び」変換は乱数なので、シナリオが再現するよう恒等にする。
        session.playful = |text| matches!(text, "l" | "1" | "I" | "O" | "0").then(|| text.to_string());
        Self {
            session,
            committed: String::new(),
            marked: String::new(),
            panel_visible: false,
            panel_line: 0,
            stale: Vec::new(),
        }
    }

    fn apply(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::SetMarked(text) => self.marked = text,
                Effect::Insert(text) => {
                    self.marked.clear();
                    self.committed.push_str(&text);
                }
                Effect::ShowCandidates { selected } => {
                    self.panel_visible = true;
                    self.panel_line = selected;
                }
                Effect::HideCandidates => self.panel_visible = false,
                Effect::RequestRerank { .. } | Effect::CancelRerank => {}
            }
        }
    }

    /// IME が処理しなかったキーを、アプリがそのまま受け取った場合の結果。
    fn pass_through(&mut self, ev: &Event) {
        if ev.control {
            return; // Ctrl+文字はアプリ側でショートカットになり、文字は入らない
        }
        for ch in ev.characters.chars() {
            match ch {
                '\r' => self.committed.push('\n'),
                '\u{7f}' | '\u{8}' => {
                    self.committed.pop();
                }
                c if c == '\u{1b}' || ('\u{F700}'..='\u{F8FF}').contains(&c) => {}
                c => self.committed.push(c),
            }
        }
    }

    /// imk.rs の handleEvent と同じ input::route でキーを処理する。
    fn press(&mut self, ev: &Event) {
        let handled = match input::route(&ev.to_input()) {
            Route::PassThrough => false,
            Route::Translit(form) => self.dispatch(Key::Translit(form), ""),
            Route::Keys(keys) => keys.iter().fold(false, |handled, (key, text)| {
                self.dispatch(*key, text) || handled
            }),
            Route::Navigation => {
                !self.session.converter.buffer().is_empty() || !self.session.composer.is_empty()
            }
        };
        if !handled {
            self.pass_through(ev);
        }
    }

    fn dispatch(&mut self, key: Key, text: &str) -> bool {
        let outcome = self.session.handle(key, text);
        self.apply(outcome.effects);
        outcome.handled
    }

    fn candidates_line(&self) -> String {
        let Phase::Converting { selected, .. } = self.session.phase() else {
            return String::new();
        };
        self.session
            .composer
            .candidates()
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let mark = if i == selected { "*" } else { "" };
                format!("{}.{}{mark}", i + 1, c.text)
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn describe_form(form: Form) -> String {
        form.name().to_string()
    }

    fn phase_name(&self) -> String {
        match self.session.phase() {
            Phase::Transliterated(form) => format!("Transliterated({})", Self::describe_form(form)),
            other => format!("{other:?}"),
        }
    }
}

fn setup_env() {
    // 既定は学習 off。`NOVAKEY_SIM_LEARNING=1` なら学習 on (記憶先は一時ディレクトリ。NOVAKEY_MEMORY_DIR で変えられる)。
    // 学習は 1 つのファイルの中に積み上がるので、`--simulate-file` の行どうしにも効く。
    let learning = std::env::var("NOVAKEY_SIM_LEARNING").is_ok_and(|v| v == "1");
    if learning {
        // SAFETY: スレッドを起こす前、simulate / simulate_file の先頭でだけ呼ぶ (setup_env)。
        unsafe { std::env::remove_var("NOVAKEY_DISABLE_LEARNING") };
    } else {
        // SAFETY: スレッドを起こす前、simulate / simulate_file の先頭でだけ呼ぶ (setup_env)。
        unsafe { std::env::set_var("NOVAKEY_DISABLE_LEARNING", "1") };
    }
    if !learning || std::env::var_os("NOVAKEY_MEMORY_DIR").is_none_or(|v| v.is_empty()) {
        let memory_dir = std::env::temp_dir().join(format!("novakey-r-sim-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&memory_dir);
        // SAFETY: スレッドを起こす前、simulate / simulate_file の先頭でだけ呼ぶ (setup_env)。
        unsafe { std::env::set_var("NOVAKEY_MEMORY_DIR", memory_dir) };
    }
    if std::env::var_os("NOVAKEY_LOANWORDS").is_none() {
        let repo_table = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/loanwords.tsv");
        // SAFETY: スレッドを起こす前、simulate / simulate_file の先頭でだけ呼ぶ (setup_env)。
        unsafe { std::env::set_var("NOVAKEY_LOANWORDS", repo_table) };
    }
    if std::env::var_os("NOVAKEY_EMOJI").is_none() {
        let repo_table = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/emoji.tsv");
        // SAFETY: スレッドを起こす前、simulate / simulate_file の先頭でだけ呼ぶ (setup_env)。
        unsafe { std::env::set_var("NOVAKEY_EMOJI", repo_table) };
    }
}

/// 1 シナリオを最後まで流す。最後に確定した文字列と、未確定のまま残った文字列を返す。
struct Run {
    committed: String,
    marked: String,
    /// 下線付き文字列が session の表示とずれた打鍵。空であるべき。
    stale: Vec<String>,
}

fn run_keys(keys: &str, verbose: bool) -> Result<Run, String> {
    let mut sim = Sim::new();
    for (label, ev) in parse_keys(keys)? {
        sim.press(&ev);
        if sim.marked != sim.session.marked_text() {
            sim.stale.push(label.clone());
        }
        if verbose {
            let panel = if sim.panel_visible { format!(" panel@{}", sim.panel_line + 1) } else { String::new() };
            println!(
                "{label:>9}  {:<34} marked={:?}{panel}  committed={:?}",
                sim.phase_name(),
                sim.marked,
                sim.committed
            );
            let cands = sim.candidates_line();
            if !cands.is_empty() {
                println!("{:>9}  {cands}", "");
            }
        }
    }
    Ok(Run { committed: sim.committed, marked: sim.marked, stale: sim.stale })
}

/// `--simulate '<keys>'`
pub fn simulate(keys: &str) {
    setup_env();
    match run_keys(keys, true) {
        Ok(Run { committed, marked, stale }) => {
            println!("final: {committed:?}");
            if !marked.is_empty() {
                println!("still composing: {marked:?}");
            }
            if !stale.is_empty() {
                println!("STALE marked text after: {stale:?}");
            }
        }
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(2);
        }
    }
}

/// 期待値の `\n` `\t` `\\` を実際の文字にする (1 行に収めるため)。
fn unescape(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        match (ch, chars.clone().next()) {
            ('\\', Some('n')) => out.push('\n'),
            ('\\', Some('t')) => out.push('\t'),
            ('\\', Some('\\')) => out.push('\\'),
            _ => {
                out.push(ch);
                continue;
            }
        }
        chars.next();
    }
    out
}

/// `--simulate-file path`: 1 行 1 シナリオ。`キー列<TAB>期待する確定文字列` なら一致を検査し、
/// 期待が無ければ結果を表示するだけ。`#` 始まりと空行は読み飛ばす。不一致があれば終了コード 1。
pub fn simulate_file(path: &str) {
    setup_env();
    let text = std::fs::read_to_string(path).unwrap_or_else(|err| {
        eprintln!("{path}: {err}");
        std::process::exit(2);
    });
    let mut failures = 0;
    let scenarios = text.lines().filter(|l| !l.trim().is_empty() && !l.starts_with('#'));
    for line in scenarios {
        let (keys, expected) = match line.split_once('\t') {
            Some((keys, expected)) => (keys, Some(unescape(expected))),
            None => (line, None),
        };
        let result = run_keys(keys, false);
        let shown = match &result {
            Ok(run) if run.marked.is_empty() => format!("{:?}", run.committed),
            Ok(run) => format!("{:?} +composing {:?}", run.committed, run.marked),
            Err(err) => format!("ERROR {err}"),
        };
        let stale = result.as_ref().is_ok_and(|run| !run.stale.is_empty());
        let verdict = match (&expected, &result) {
            _ if stale => {
                failures += 1;
                "STAL"
            }
            (None, _) => "    ",
            (Some(want), Ok(run)) if run.marked.is_empty() && run.committed == *want => "ok  ",
            _ => {
                failures += 1;
                "FAIL"
            }
        };
        let want = expected.filter(|_| verdict != "ok  ").map_or(String::new(), |w| format!("   want {w:?}"));
        println!("{verdict} {keys}  =>  {shown}{want}");
    }
    if failures > 0 {
        eprintln!("{failures} scenario(s) failed");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 通常の文字とトークンを打鍵列に分ける() {
        let keys = parse_keys("a<S-space><C-k><lt>").unwrap();
        let labels: Vec<_> = keys.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, ["a", "<S-space>", "<C-k>", "<lt>"]);
        assert!(keys[1].1.shift && keys[2].1.control);
    }

    #[test]
    fn 未知のトークンはエラー() {
        assert!(parse_keys("<nope>").is_err());
    }

    #[test]
    fn 閉じない山括弧は文字として扱う() {
        assert_eq!(parse_keys("<").unwrap().len(), 1);
    }
}
