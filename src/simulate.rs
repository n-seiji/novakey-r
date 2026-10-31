#![forbid(unsafe_code)]

// simulate.rs - `--simulate '<keys>'`: 実際の打鍵経路 (input::route → Controller) を headless で動かす。
// 変換は本物の azooKey、英単語は本物の loanwords.tsv と emoji.tsv。Cocoa には触れない。
//
// キー表記: 通常の文字はそのまま。<space> <S-space> <enter> <esc> <bs> <tab> <up> <down> <left> <right>
// <C-k> (Ctrl+文字) <F6>..<F10> <lt> (`<`) <gt> (`>`)。
//
// IMK 側のイベント (番号は candidates 表示と同じ 1 起点):
// <pick:N> 候補パネルで N 番目をクリック / <highlight:N> パネルのハイライトを N 番目へ /
// <live> <space-mode> 変換モードをライブ変換 / スペース変換へ / <commit-all> 入力ソース切替などで組成を確定。

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use crate::composer::Phase;
use crate::controller::{new_session, CandidatePanel, Controller, Environment, TextClient};
use crate::input::{self, KeyInput, Modifiers};
use crate::session::ConversionMode;

fn key_input(key_code: u16, characters: &str) -> KeyInput {
    KeyInput {
        key_code,
        characters: characters.to_string(),
        // sim では characters と ignoring を区別しない (実機の characters は Ctrl で "\u{b}" になる)。
        characters_ignoring_modifiers: characters.to_string(),
        mods: Modifiers::default(),
    }
}

fn token_key(name: &str) -> Result<KeyInput, String> {
    let key = match name {
        "space" => key_input(49, " "),
        "S-space" => KeyInput { mods: Modifiers { shift: true, ..Modifiers::default() }, ..key_input(49, " ") },
        "enter" => key_input(36, "\r"),
        "esc" => key_input(53, "\u{1b}"),
        "bs" => key_input(51, "\u{7f}"),
        "tab" => key_input(48, "\t"),
        "up" => key_input(126, "\u{F700}"),
        "down" => key_input(125, "\u{F701}"),
        "left" => key_input(123, "\u{F702}"),
        "right" => key_input(124, "\u{F703}"),
        "lt" => key_input(0, "<"),
        "gt" => key_input(0, ">"),
        "F6" => key_input(97, "\u{F709}"),
        "F7" => key_input(98, "\u{F70A}"),
        "F8" => key_input(100, "\u{F70B}"),
        "F9" => key_input(101, "\u{F70C}"),
        "F10" => key_input(109, "\u{F70D}"),
        _ => {
            let ctrl = name
                .strip_prefix("C-")
                .filter(|rest| rest.chars().count() == 1)
                .ok_or_else(|| format!("unknown key token <{name}>"))?;
            KeyInput { mods: Modifiers { control: true, ..Modifiers::default() }, ..key_input(0, ctrl) }
        }
    };
    Ok(key)
}

/// シナリオ 1 手。物理キー (imk.rs の handleEvent が受け取るものと同じ粒度) か、IMK のコールバックの再現。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Key(KeyInput),
    Pick(usize),
    Highlight(usize),
    Live,
    SpaceMode,
    CommitAll,
}

fn token_step(name: &str) -> Result<Step, String> {
    let number = |arg: &str| {
        arg.parse::<usize>().ok().filter(|&n| n >= 1).ok_or_else(|| format!("bad number in <{name}>"))
    };
    let step = match name.split_once(':') {
        Some(("pick", arg)) => Step::Pick(number(arg)?),
        Some(("highlight", arg)) => Step::Highlight(number(arg)?),
        _ => match name {
            "live" => Step::Live,
            "space-mode" => Step::SpaceMode,
            "commit-all" => Step::CommitAll,
            _ => Step::Key(token_key(name)?),
        },
    };
    Ok(step)
}

fn parse_keys(keys: &str) -> Result<Vec<(String, Step)>, String> {
    let mut events = Vec::new();
    let mut rest = keys;
    while let Some(ch) = rest.chars().next() {
        let named = (ch == '<')
            .then(|| rest.find('>').map(|end| &rest[1..end]))
            .flatten();
        if let Some(name) = named {
            events.push((format!("<{name}>"), token_step(name)?));
            rest = &rest[name.len() + 2..];
        } else {
            let mut key = key_input(0, &ch.to_string());
            key.mods.shift = ch.is_ascii_uppercase();
            events.push((ch.to_string(), Step::Key(key)));
            rest = &rest[ch.len_utf8()..];
        }
    }
    Ok(events)
}

/// ホストアプリの代わり。確定済みの文字列と、下線付きの未確定文字列 (最後の SetMarked) を持つ。
#[derive(Clone, Default)]
pub(crate) struct FakeClient(Rc<RefCell<ClientState>>);

#[derive(Default)]
pub(crate) struct ClientState {
    pub committed: String,
    pub marked: String,
    pub bundle: Option<String>,
}

impl TextClient for FakeClient {
    fn insert(&self, text: &str) {
        let mut state = self.0.borrow_mut();
        state.marked.clear();
        state.committed.push_str(text);
    }

    fn set_marked(&self, text: &str) {
        self.0.borrow_mut().marked = text.to_string();
    }

    fn bundle_id(&self) -> Option<String> {
        self.0.borrow().bundle.clone()
    }
}

impl FakeClient {
    pub(crate) fn committed(&self) -> String {
        self.0.borrow().committed.clone()
    }

    pub(crate) fn marked(&self) -> String {
        self.0.borrow().marked.clone()
    }

    pub(crate) fn set_bundle(&self, id: &str) {
        self.0.borrow_mut().bundle = Some(id.to_string());
    }

    fn push_committed(&self, ch: char) {
        self.0.borrow_mut().committed.push(ch);
    }

    fn pop_committed(&self) {
        self.0.borrow_mut().committed.pop();
    }
}

/// 候補パネルの代わり。表示中か、ハイライトの行と、呼ばれた順の記録を持つ。
#[derive(Clone, Default)]
pub(crate) struct FakePanel(Rc<RefCell<PanelState>>);

#[derive(Default)]
pub(crate) struct PanelState {
    pub visible: bool,
    pub line: usize,
    pub calls: Vec<String>,
}

impl CandidatePanel for FakePanel {
    fn show(&self) -> bool {
        let mut state = self.0.borrow_mut();
        state.calls.push("show".to_string());
        let reread = !state.visible;
        if reread {
            state.line = 0;
        }
        state.visible = true;
        reread
    }

    fn hide(&self) {
        let mut state = self.0.borrow_mut();
        state.calls.push("hide".to_string());
        state.visible = false;
    }

    fn move_highlight(&self, delta: isize) {
        let mut state = self.0.borrow_mut();
        state.calls.push(format!("move:{delta}"));
        state.line = (state.line as isize + delta) as usize;
    }
}

impl FakePanel {
    pub(crate) fn visible(&self) -> bool {
        self.0.borrow().visible
    }

    pub(crate) fn line(&self) -> usize {
        self.0.borrow().line
    }

    #[cfg(test)]
    pub(crate) fn calls(&self) -> Vec<String> {
        self.0.borrow().calls.clone()
    }
}

/// 環境の代わり。セキュア入力はなく、ログ・トレースは捨てる。
#[derive(Clone, Default)]
pub(crate) struct FakeEnv(Rc<RefCell<EnvState>>);

#[derive(Default)]
pub(crate) struct EnvState {
    /// ライブ変換の全体設定。ユーザー defaults の代わり (既定は Space = false)。
    pub live: bool,
    /// `set_composing` に渡された値の履歴。
    pub composing: Vec<bool>,
}

impl FakeEnv {
    #[cfg(test)]
    pub(crate) fn last_composing(&self) -> Option<bool> {
        self.0.borrow().composing.last().copied()
    }
}

impl Environment for FakeEnv {
    fn secure_input(&self) -> bool {
        false
    }

    fn live_conversion(&self) -> bool {
        self.0.borrow().live
    }

    fn set_live_conversion(&self, enabled: bool) {
        self.0.borrow_mut().live = enabled;
    }

    fn set_composing(&self, composing: bool) {
        self.0.borrow_mut().composing.push(composing);
    }

    fn log(&self, _line: &str) {}

    fn trace(&self, _entry: &str, _fields: &str) {}
}

pub(crate) type SimController = Controller<FakeClient, FakePanel, FakeEnv>;

/// 本物のアプリが持つ状態のうち、キー処理の結果として見えるもの。
struct Sim {
    controller: SimController,
    client: FakeClient,
    panel: FakePanel,
}

impl Sim {
    fn new() -> Self {
        let session = new_session(ConversionMode::Space);
        let panel = FakePanel::default();
        Self {
            controller: Controller::new(0, session, panel.clone(), FakeEnv::default()),
            client: FakeClient::default(),
            panel,
        }
    }

    /// IME が処理しなかったキーを、アプリがそのまま受け取った場合の結果。
    fn pass_through(&mut self, key: &KeyInput) {
        if key.mods.control {
            return; // Ctrl+文字はアプリ側でショートカットになり、文字は入らない
        }
        for ch in key.characters.chars() {
            match ch {
                '\r' => self.client.push_committed('\n'),
                '\u{7f}' | '\u{8}' => self.client.pop_committed(),
                c if c == '\u{1b}' || ('\u{F700}'..='\u{F8FF}').contains(&c) => {}
                c => self.client.push_committed(c),
            }
        }
    }

    /// imk.rs の handleEvent と同じ input::route と Controller でキーを処理する。
    fn press(&mut self, key: &KeyInput) {
        if !self.controller.on_key(input::route(key), self.client.clone()) {
            self.pass_through(key);
        }
    }

    /// 1 起点の番号 n の候補の文字列。無ければエラー。
    fn candidate_at(&self, label: &str, n: usize) -> Result<String, String> {
        self.controller.candidate_texts().get(n - 1).cloned().ok_or_else(|| format!("{label}: no candidate {n}"))
    }

    fn apply(&mut self, label: &str, step: &Step) -> Result<(), String> {
        match step {
            Step::Key(key) => self.press(key),
            Step::Pick(n) => {
                let text = self.candidate_at(label, *n)?;
                self.controller.on_candidate_selected(&text);
            }
            Step::Highlight(n) => {
                let text = self.candidate_at(label, *n)?;
                self.controller.on_selection_changed(&text);
            }
            Step::Live => {
                if self.controller.mode() != ConversionMode::Live {
                    self.controller.on_toggle_conversion_mode();
                }
            }
            Step::SpaceMode => {
                if self.controller.mode() == ConversionMode::Live {
                    self.controller.on_toggle_conversion_mode();
                }
            }
            Step::CommitAll => self.controller.on_commit_composition(Some(self.client.clone())),
        }
        Ok(())
    }

    fn candidates_line(&self) -> String {
        let Phase::Converting { selected, .. } = self.controller.phase() else {
            return String::new();
        };
        self.controller
            .candidate_texts()
            .iter()
            .enumerate()
            .map(|(i, text)| {
                let mark = if i == selected { "*" } else { "" };
                format!("{}.{text}{mark}", i + 1)
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn phase_name(&self) -> String {
        match self.controller.phase() {
            Phase::Transliterated(form) => format!("Transliterated({})", form.name()),
            other => format!("{other:?}"),
        }
    }
}

pub(crate) fn setup_env() {
    // 既定は学習 off。`SHIZUKU_SIM_LEARNING=1` なら学習 on (記憶先は一時ディレクトリ。SHIZUKU_MEMORY_DIR で変えられる)。
    // 学習は 1 つのファイルの中に積み上がるので、`--simulate-file` の行どうしにも効く。
    let learning = std::env::var("SHIZUKU_SIM_LEARNING").is_ok_and(|v| v == "1");
    let memory_dir = match std::env::var_os("SHIZUKU_MEMORY_DIR") {
        Some(dir) if learning && !dir.is_empty() => PathBuf::from(dir),
        _ => {
            let dir = std::env::temp_dir().join(format!("shizuku-sim-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            dir
        }
    };
    let repo_table = |env: &str, name: &str| {
        std::env::var_os(env).map_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources").join(name), PathBuf::from)
    };
    crate::config::init(crate::config::Config {
        memory_dir: Some(memory_dir),
        learning_enabled: learning,
        loanwords_path: Some(repo_table("SHIZUKU_LOANWORDS", "loanwords.tsv")),
        dev_words_path: Some(repo_table("SHIZUKU_DEV_WORDS", "dev_words.txt")),
        emoji_path: Some(repo_table("SHIZUKU_EMOJI", "emoji.tsv")),
        // シミュレータは SHIZUKU_JUDGE_DIR を指定したときだけ審判を読む。
        judge_dir: std::env::var_os("SHIZUKU_JUDGE_DIR").map(PathBuf::from),
    });
}

thread_local! {
    /// 打鍵ごとの処理時間 (ms) と、審判が走ったか。`SHIZUKU_SIM_TIMING=1` の要約用。
    static TIMINGS: std::cell::RefCell<Vec<(f64, bool)>> = const { std::cell::RefCell::new(Vec::new()) };
    /// 審判だけの時間 (ms)。
    static JUDGE_MS: std::cell::RefCell<Vec<f64>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn print_timing_summary() {
    let mut all: Vec<f64> = Vec::new();
    let mut judged: Vec<f64> = Vec::new();
    TIMINGS.with(|t| {
        for &(ms, j) in t.borrow().iter() {
            all.push(ms);
            if j {
                judged.push(ms);
            }
        }
    });
    let pct = |v: &mut Vec<f64>, p: f64| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        v.get(((v.len() as f64 - 1.0) * p).round() as usize).copied().unwrap_or(0.0)
    };
    let rss = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<f64>().ok())
        .unwrap_or(0.0)
        / 1024.0;
    let mut jm = JUDGE_MS.with(|j| j.borrow().clone());
    println!(
        "judge: n={} median {:.1} ms p90 {:.1} ms max {:.1} ms",
        jm.len(),
        pct(&mut jm, 0.5),
        pct(&mut jm, 0.9),
        pct(&mut jm, 1.0)
    );
    if let Some((bytes, ok)) = crate::judge::locked() {
        println!("judge-locked mb={} ok={}", bytes >> 20, u8::from(ok));
    }
    println!(
        "timing: keys={} median {:.1} ms p95 {:.1} ms max {:.1} ms | keys with judge={} median {:.1} ms p95 {:.1} ms max {:.1} ms | rss {:.0} MB",
        all.len(),
        pct(&mut all, 0.5),
        pct(&mut all, 0.95),
        pct(&mut all, 1.0),
        judged.len(),
        pct(&mut judged, 0.5),
        pct(&mut judged, 0.95),
        pct(&mut judged, 1.0),
        rss
    );
}

/// 1 シナリオを最後まで流す。最後に確定した文字列と、未確定のまま残った文字列を返す。
struct Run {
    committed: String,
    marked: String,
    /// 下線付き文字列が session の表示とずれた打鍵。空であるべき。
    stale: Vec<String>,
    /// 表示と確定される文字列がずれた打鍵 (検査を有効にしたときだけ入る)。空であるべき。
    wysiwyg_broken: Vec<String>,
}

fn run_keys(keys: &str, verbose: bool, check_preedit: bool) -> Result<Run, String> {
    // 行どうしが文脈を持ち越さないようにする。アプリは 1 つ (bundle id `sim`) に固定。
    crate::context::with(|store| store.clear_all());
    let mut sim = Sim::new();
    sim.client.set_bundle("sim");
    let mut stale = Vec::new();
    // 検査は opt-in。読みに取り込まれない未確定ローマ字が残るとき (`kyout<enter>` → `kyout`) は仕様で一致しないので見ない (n だけなら ん として一致するので見る)。
    let mut wysiwyg_broken = Vec::new();
    for (label, step) in parse_keys(keys)? {
        let started = std::time::Instant::now();
        sim.apply(&label, &step)?;
        let judge_ms = crate::judge::take_last_ms();
        if let Some(ms) = judge_ms {
            JUDGE_MS.with(|j| j.borrow_mut().push(ms));
        }
        if matches!(step, Step::Key(_)) {
            TIMINGS.with(|t| t.borrow_mut().push((started.elapsed().as_secs_f64() * 1e3, judge_ms.is_some())));
        }
        if sim.client.marked() != sim.controller.marked_text() {
            stale.push(label.clone());
        }
        if check_preedit && sim.controller.commit_preview().is_some_and(|p| p != sim.controller.marked_text()) {
            wysiwyg_broken.push(label.clone());
        }
        if verbose {
            let panel = if sim.panel.visible() { format!(" panel@{}", sim.panel.line() + 1) } else { String::new() };
            println!(
                "{label:>9}  {:<34} marked={:?}{panel}  committed={:?}",
                sim.phase_name(),
                sim.client.marked(),
                sim.client.committed()
            );
            if let Some(ms) = judge_ms {
                println!("{:>9}  judge {ms:.1} ms", "");
            }
            let cands = sim.candidates_line();
            if !cands.is_empty() {
                println!("{:>9}  {cands}", "");
            }
        }
    }
    Ok(Run { committed: sim.client.committed(), marked: sim.client.marked(), stale, wysiwyg_broken })
}

/// `--simulate '<keys>'`
pub fn simulate(keys: &str, check_preedit: bool) {
    setup_env();
    match run_keys(keys, true, check_preedit) {
        Ok(Run { committed, marked, stale, wysiwyg_broken }) => {
            println!("final: {committed:?}");
            if !marked.is_empty() {
                println!("still composing: {marked:?}");
            }
            if !stale.is_empty() {
                println!("STALE marked text after: {stale:?}");
            }
            if !wysiwyg_broken.is_empty() {
                println!("WYSIWYG preedit differs from commit preview after: {wysiwyg_broken:?}");
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
pub fn simulate_file(path: &str, check_preedit: bool) {
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
        let result = run_keys(keys, false, check_preedit);
        let shown = match &result {
            Ok(run) if run.marked.is_empty() => format!("{:?}", run.committed),
            Ok(run) => format!("{:?} +composing {:?}", run.committed, run.marked),
            Err(err) => format!("ERROR {err}"),
        };
        let stale = result.as_ref().is_ok_and(|run| !run.stale.is_empty());
        let wysiwyg = result.as_ref().is_ok_and(|run| !run.wysiwyg_broken.is_empty());
        let verdict = match (&expected, &result) {
            _ if stale => {
                failures += 1;
                "STAL"
            }
            _ if wysiwyg => {
                failures += 1;
                "WYSI"
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
    if std::env::var_os("SHIZUKU_SIM_TIMING").is_some() {
        print_timing_summary();
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
        assert!(matches!(&keys[1].1, Step::Key(key) if key.mods.shift));
        assert!(matches!(&keys[2].1, Step::Key(key) if key.mods.control));
    }

    #[test]
    fn imk_イベントのトークンを解釈する() {
        let steps: Vec<_> = parse_keys("<pick:2><highlight:3><live><space-mode><commit-all>")
            .unwrap()
            .into_iter()
            .map(|(_, s)| s)
            .collect();
        assert_eq!(
            steps,
            [
                Step::Pick(2),
                Step::Highlight(3),
                Step::Live,
                Step::SpaceMode,
                Step::CommitAll,
            ]
        );
    }

    #[test]
    fn 番号が不正なトークンはエラー() {
        assert!(parse_keys("<pick:0>").is_err());
        assert!(parse_keys("<pick:x>").is_err());
        assert!(parse_keys("<pick>").is_err());
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
