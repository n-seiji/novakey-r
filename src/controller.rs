#![forbid(unsafe_code)]

//! キー処理の結果 (Effect) を画面側へ反映する層。Cocoa には触れず、ホストアプリ・候補パネル・環境は trait の向こうにある。
//! 本番は imk.rs / imk_client.rs が実装し、simulate.rs と下のテストは fake を差す。
//!
//! 借用の規則: `session` の RefCell を借りたまま TextClient / CandidatePanel / Environment を呼ばない。
//! 本物のパネルは show / move_highlight の中から candidates: などを同期で呼び戻し、session を再借用する。

use std::cell::{Cell, OnceCell, RefCell};

use crate::composer::{Composer, Phase};
use crate::input::Route;
use crate::keymap::Key;
use crate::kkc::{self, RustEngine};
use crate::outbound::{self, Outbound, Snapshot};
use crate::session::{ConversionMode, Effect, Outcome, RerankJob, Session};

/// 直前文脈として読む最大長 (UTF-16 単位)。
const CONTEXT_MAX_UTF16: usize = 200;

/// ホストアプリのテキスト入力。
pub trait TextClient {
    fn insert(&self, text: &str);
    fn set_marked(&self, text: &str);
    /// マーク範囲 (なければ選択範囲) の直前のテキスト。取れなければ空。
    fn preceding_text(&self, max_utf16: usize) -> String;
    fn bundle_id(&self) -> Option<String>;
}

/// 候補パネル。行番号は問い合わせず、Controller が追跡する。
pub trait CandidatePanel {
    /// 出す。非表示から読み直したなら true (ハイライトは先頭に戻る)。
    fn show(&self) -> bool;
    fn hide(&self);
    fn move_highlight(&self, delta: isize);
}

/// アプリ全体の状態・外部への出口。
pub trait Environment {
    fn secure_input(&self) -> bool;
    /// AI 並べ替えが有効で、認証情報も揃っているか。
    fn rerank_allowed(&self) -> bool;
    fn log(&self, line: &str);
    /// 名前と数値だけを渡すこと。
    fn trace(&self, entry: &str, fields: &str);
    fn send_rerank(&self, controller_id: u64, job: &RerankJob, context: &str, urgent: bool);
    fn cancel_rerank(&self, controller_id: u64);
}

/// `generation` は AI 結果の世代番号の開始値。作り直した session が、古い結果と同じ番号を再利用しないように渡す。
pub fn new_session(mode: ConversionMode, generation: u64) -> Session<RustEngine> {
    Session::new(
        Composer::new(RustEngine::new())
            .with_generation(generation)
            .with_english(crate::loanword::english_for)
            .with_english_word(crate::loanword::is_english_word)
            .with_english_memory(crate::english_memory::knows, crate::english_memory::learn)
            .with_emoji(crate::emoji::emoji_for),
        mode,
    )
}

/// 生きている間 `flag` を true にし、panic で巻き戻っても false に戻す。
struct FlagGuard<'a>(&'a Cell<bool>);

impl<'a> FlagGuard<'a> {
    fn set(flag: &'a Cell<bool>) -> Self {
        flag.set(true);
        Self(flag)
    }
}

impl Drop for FlagGuard<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

pub struct Controller<C, P, E> {
    id: u64,
    session: RefCell<Session<RustEngine>>,
    panel: P,
    env: E,
    last_client: RefCell<Option<C>>,
    /// show_candidates がパネルを更新している間 true。パネルが同期で呼び戻す選択変更を無視する。
    syncing: Cell<bool>,
    /// 候補パネルがハイライトしている行。パネルの行番号 API は使えないので、こちらで追跡する。
    panel_line: Cell<usize>,
    /// クライアントの bundle id。コントローラは 1 つのクライアントに付くので、最初に取れたものを使い回す。
    /// 取れなかったときは入れず、次の機会に取り直す。
    bundle_id: OnceCell<String>,
    /// 直前に外へ出した「送らなかった / 文脈なしで送った」理由。同じ理由を打鍵ごとにログしないために持つ。
    last_gate: Cell<Option<(&'static str, &'static str)>>,
}

impl<C: TextClient + Clone, P: CandidatePanel, E: Environment> Controller<C, P, E> {
    pub fn new(id: u64, session: Session<RustEngine>, panel: P, env: E) -> Self {
        Self {
            id,
            session: RefCell::new(session),
            panel,
            env,
            last_client: RefCell::new(None),
            syncing: Cell::new(false),
            panel_line: Cell::new(0),
            bundle_id: OnceCell::new(),
            last_gate: Cell::new(None),
        }
    }

    pub fn phase(&self) -> Phase {
        self.session.borrow().phase()
    }

    pub fn mode(&self) -> ConversionMode {
        self.session.borrow().mode()
    }

    /// パネル更新中か。呼び戻された選択変更は無視するので、呼び出し側は Cocoa の値を読む前に見てよい。
    pub fn is_syncing(&self) -> bool {
        self.syncing.get()
    }

    pub fn candidate_texts(&self) -> Vec<String> {
        self.session.borrow().candidate_texts()
    }

    pub fn marked_text(&self) -> String {
        self.session.borrow().marked_text()
    }

    fn client(&self) -> Option<C> {
        self.last_client.borrow().clone()
    }

    /// キー 1 打。処理したら true (false ならホストアプリへ素通しする)。
    pub fn on_key(&self, route: Route, client: C) -> bool {
        if route != Route::PassThrough {
            *self.last_client.borrow_mut() = Some(client.clone());
        }
        match route {
            Route::PassThrough => false,
            Route::Translit(form) => self.dispatch(Key::Translit(form), "", &client),
            Route::Keys(keys) => keys.iter().fold(false, |handled, (key, text)| {
                self.dispatch(*key, text, &client) || handled
            }),
            Route::Navigation => {
                let has_pending = self.session.borrow().has_pending();
                self.env.trace(
                    "handleEvent",
                    &format!("key=Navigation phase={:?} handled={has_pending}", self.phase()),
                );
                has_pending
            }
        }
    }

    /// パネルのハイライトが動いた。
    pub fn on_selection_changed(&self, text: &str) {
        if self.syncing.get() {
            return;
        }
        let Some(client) = self.client() else { return };
        self.env.trace("candidateSelectionChanged", &format!("phase={:?}", self.phase()));
        let effects = self.session.borrow_mut().select_highlight(text);
        if let Phase::Converting { selected, .. } = self.phase() {
            self.panel_line.set(selected);
        }
        self.run(effects, Some(&client));
    }

    /// パネルで候補が選ばれた。
    pub fn on_candidate_selected(&self, text: &str) {
        let Some(client) = self.client() else { return };
        self.env.trace("candidateSelected", &format!("phase={:?}", self.phase()));
        self.sync_learning_flag(Some(&client));
        let effects = self.session.borrow_mut().commit_selected(text);
        self.run(effects, Some(&client));
    }

    pub fn on_commit_composition(&self, client: Option<C>) {
        self.sync_rerank_flag();
        self.sync_learning_flag(client.as_ref());
        let effects = self.session.borrow_mut().commit_composition();
        self.run(effects, client.as_ref());
    }

    pub fn on_toggle_conversion_mode(&self) {
        let effects = {
            let mut session = self.session.borrow_mut();
            let live = session.mode() != ConversionMode::Live;
            session.set_live(live)
        };
        if let Some(client) = self.client() {
            self.run(effects, Some(&client));
        }
    }

    /// AI 並べ替えの結果 (選ばれた候補の番号)。
    pub fn on_rerank_result(&self, generation: u64, index: usize) {
        let Some(client) = self.client() else { return };
        let effects = {
            // 変換処理の途中に run loop が再入した場合は、結果を捨てる (借用の二重取得で落とさない)。
            let Ok(mut session) = self.session.try_borrow_mut() else {
                return;
            };
            session.apply_rerank(generation, index)
        };
        self.run(effects, Some(&client));
    }

    /// panic で session が中途半端かもしれないので、現在のモードで作り直して画面側も揃える。
    /// 呼び出し側 (imk.rs) が、これ自体の panic を止める。
    pub fn reset_after_panic(&self) {
        match self.session.try_borrow_mut() {
            Ok(mut session) => {
                // 作り直した session にも、メニューの表示と同じ現在のモードを引き継ぐ。
                *session = new_session(session.mode(), session.generation());
            }
            Err(_) => self.env.log("reset_after_panic: session is borrowed, not recreated"),
        }
        self.env.cancel_rerank(self.id);
        self.panel.hide();
        self.syncing.set(false);
        self.panel_line.set(0);
        if let Some(client) = self.client() {
            client.set_marked("");
        }
    }

    /// 無効時は RequestRerank が作られず、Keychain に触れる経路へ進まない。
    fn sync_rerank_flag(&self) {
        let allowed = self.env.rerank_allowed();
        self.session.borrow_mut().set_rerank(allowed);
    }

    /// 確定が起きうる処理の前に、学習してよい場面かを kkc に伝える。
    fn sync_learning_flag(&self, client: Option<&C>) {
        let bundle_id = self.bundle_id(client);
        kkc::set_learning_allowed(outbound::learning_allowed(self.env.secure_input(), bundle_id));
    }

    /// クライアントの bundle id (学習判定と AI 送信判定で共通)。最初に取れたものを使い回す。
    fn bundle_id(&self, client: Option<&C>) -> Option<&str> {
        if self.bundle_id.get().is_none() {
            let fetched = match client {
                Some(client) => client.bundle_id(),
                None => self.client().and_then(|c| c.bundle_id()),
            };
            if let Some(id) = fetched {
                let _ = self.bundle_id.set(id);
            }
        }
        self.bundle_id.get().map(String::as_str)
    }

    /// session に判断させ、返ってきた Effect を実行する。処理したら true。
    fn dispatch(&self, key: Key, text: &str, client: &C) -> bool {
        self.sync_rerank_flag();
        self.sync_learning_flag(Some(client));
        // session の借用はここで外れる。以降のポート呼び出しは composer を再借用してよい。
        let outcome = self.session.borrow_mut().handle(key, text);
        let Outcome { handled, action, phase, effects } = outcome;
        self.run(effects, Some(client));
        match action {
            None => self.env.trace(
                "handleEvent",
                &format!("key={} phase={phase:?} action=Romaji handled={handled}", key.name()),
            ),
            Some(action) => self.env.trace(
                "handleEvent",
                &format!(
                    "key={} phase={phase:?} action={} handled={handled} now={:?}",
                    key.name(),
                    action.name(),
                    self.phase()
                ),
            ),
        }
        handled
    }

    fn run(&self, effects: Vec<Effect>, client: Option<&C>) {
        for effect in effects {
            match effect {
                Effect::SetMarked(text) => {
                    if let Some(client) = client {
                        client.set_marked(&text);
                    }
                }
                Effect::Insert(text) => {
                    if let Some(client) = client {
                        client.insert(&text);
                    }
                }
                Effect::ShowCandidates { selected } => self.show_candidates(selected),
                Effect::HideCandidates => self.panel.hide(),
                Effect::RequestRerank { urgent, job } => self.send_rerank(urgent, &job),
                Effect::CancelRerank => self.env.cancel_rerank(self.id),
            }
        }
    }

    /// 候補ウィンドウを出してハイライトを合わせる。
    fn show_candidates(&self, selected: usize) {
        // パネルは candidates: / candidateSelectionChanged: を同期で呼び戻し、session を再借用する。
        // ここでは借用を持っていない。
        let syncing = FlagGuard::set(&self.syncing);
        if self.panel.show() {
            self.panel_line.set(0);
        }
        let delta = selected as isize - self.panel_line.get() as isize;
        self.panel.move_highlight(delta);
        self.panel_line.set(selected);
        drop(syncing);
        self.env.trace("select", &format!("line={selected} moved={delta}"));
    }

    /// urgent は Space 押下時用。デバウンスを待たずに送る(送信済みなら重複しない)。
    fn send_rerank(&self, urgent: bool, job: &RerankJob) {
        let client = self.client();
        let preceding = client
            .as_ref()
            .map(|c| c.preceding_text(CONTEXT_MAX_UTF16))
            .unwrap_or_default();
        let snapshot = Snapshot {
            secure_input: self.env.secure_input(),
            bundle_id: self.bundle_id(client.as_ref()),
            preceding: &preceding,
            reading: &job.hiragana,
            candidates: &job.candidates,
        };
        let context = match outbound::decide(&snapshot) {
            Outbound::Skip(reason) => {
                self.log_gate(Some(("skipped", reason.name())));
                // デバウンス中の古い要求が、外へ出せない文脈のまま送られないようにする。
                self.env.cancel_rerank(self.id);
                return;
            }
            Outbound::WithoutContext(reason) => {
                self.log_gate(Some(("without context", reason.name())));
                String::new()
            }
            Outbound::WithContext(context) => {
                self.log_gate(None);
                context
            }
        };
        self.env.send_rerank(self.id, job, &context, urgent);
    }

    /// 理由が前回と変わったときだけログする (打鍵ごとに同じ行が出ないように)。名前だけで入力は含まない。
    fn log_gate(&self, gate: Option<(&'static str, &'static str)>) {
        if self.last_gate.replace(gate) != gate {
            if let Some((kind, reason)) = gate {
                self.env.log(&format!("LLM rerank {kind}: {reason}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    use crate::simulate::{FakeClient, FakeEnv, FakePanel};

    type Fixture = (Controller<FakeClient, FakePanel, FakeEnv>, FakeClient, FakePanel);

    fn fixture() -> Fixture {
        let panel = FakePanel::default();
        let controller = Controller::new(1, new_session(ConversionMode::Space, 0), panel.clone(), FakeEnv::default());
        (controller, FakeClient::default(), panel)
    }

    fn press(f: &Fixture, key: Key, text: &str) -> bool {
        f.0.on_key(Route::Keys(vec![(key, text.to_string())]), f.1.clone())
    }

    fn type_letters(f: &Fixture, letters: &str) {
        for ch in letters.chars() {
            assert!(press(f, Key::Letter { upper: false }, &ch.to_string()));
        }
    }

    #[test]
    fn 打鍵するとクライアントの未確定文字列が更新される() {
        let f = fixture();
        type_letters(&f, "a");
        assert_eq!(f.1.marked(), "あ");
        assert_eq!(f.1.committed(), "");
    }

    #[test]
    fn 変換を始めるとパネルが出て_確定するとクライアントに挿入されパネルが隠れる() {
        let f = fixture();
        type_letters(&f, "a");
        assert!(press(&f, Key::Space, " "));
        assert!(!f.2.visible(), "1 回目のスペースは窓なしで変換する");
        assert!(press(&f, Key::Space, " "));
        assert!(f.2.visible());
        assert!(f.2.calls().contains(&"show".to_string()));
        assert!(press(&f, Key::Enter, "\r"));
        assert!(!f.2.visible());
        assert_eq!(f.1.marked(), "");
        assert!(!f.1.committed().is_empty());
        assert_eq!(f.2.calls().last().map(String::as_str), Some("hide"));
    }

    #[test]
    fn 下キーで次の候補にハイライトが動く() {
        let f = fixture();
        type_letters(&f, "a");
        press(&f, Key::Space, " ");
        press(&f, Key::Space, " ");
        let before = f.2.line();
        assert!(press(&f, Key::Down, ""));
        assert_eq!(f.2.line(), before + 1);
        assert!(f.2.calls().contains(&"move:1".to_string()));
    }

    #[test]
    fn 未確定文があるときだけナビゲーションキーを握りつぶす() {
        let f = fixture();
        assert!(!f.0.on_key(Route::Navigation, f.1.clone()));
        type_letters(&f, "a");
        assert!(f.0.on_key(Route::Navigation, f.1.clone()));
        assert_eq!(f.1.marked(), "あ");
    }

    #[test]
    fn 素通しのキーはクライアントを更新せず_処理しなかったと返す() {
        let f = fixture();
        assert!(!f.0.on_key(Route::PassThrough, f.1.clone()));
        assert_eq!(f.1.marked(), "");
    }

    #[test]
    fn ライブ変換の切り替えはモードを反転する() {
        let f = fixture();
        f.0.on_key(Route::Navigation, f.1.clone());
        assert_eq!(f.0.mode(), ConversionMode::Space);
        f.0.on_toggle_conversion_mode();
        assert_eq!(f.0.mode(), ConversionMode::Live);
    }

    #[derive(Clone, Default)]
    struct RecClient {
        bundle: Rc<RefCell<Option<String>>>,
        bundle_calls: Rc<Cell<usize>>,
        marked: Rc<RefCell<String>>,
    }

    impl RecClient {
        fn set_bundle(&self, id: Option<&str>) {
            *self.bundle.borrow_mut() = id.map(str::to_string);
        }
    }

    impl TextClient for RecClient {
        fn insert(&self, _text: &str) {
            self.marked.borrow_mut().clear();
        }
        fn set_marked(&self, text: &str) {
            *self.marked.borrow_mut() = text.to_string();
        }
        fn preceding_text(&self, _max_utf16: usize) -> String {
            String::new()
        }
        fn bundle_id(&self) -> Option<String> {
            self.bundle_calls.set(self.bundle_calls.get() + 1);
            self.bundle.borrow().clone()
        }
    }

    #[derive(Clone, Default)]
    struct RecEnv {
        allowed: Rc<Cell<bool>>,
        sent: Rc<RefCell<Vec<u64>>>,
        cancels: Rc<Cell<usize>>,
        logs: Rc<RefCell<Vec<String>>>,
    }

    impl Environment for RecEnv {
        fn secure_input(&self) -> bool {
            false
        }
        fn rerank_allowed(&self) -> bool {
            self.allowed.get()
        }
        fn log(&self, line: &str) {
            self.logs.borrow_mut().push(line.to_string());
        }
        fn trace(&self, _entry: &str, _fields: &str) {}
        fn send_rerank(&self, _id: u64, job: &RerankJob, _context: &str, _urgent: bool) {
            self.sent.borrow_mut().push(job.generation);
        }
        fn cancel_rerank(&self, _id: u64) {
            self.cancels.set(self.cancels.get() + 1);
        }
    }

    /// show / move_highlight の中で呼ぶフックを持つパネル (本物のパネルが同期で呼び戻すのを真似る)。
    #[derive(Clone, Default)]
    struct HookPanel {
        hook: Rc<RefCell<Option<Rc<dyn Fn()>>>>,
        hooks_run: Rc<Cell<usize>>,
        hidden: Rc<Cell<usize>>,
    }

    impl HookPanel {
        fn fire(&self) {
            let hook = self.hook.borrow().clone();
            if let Some(hook) = hook {
                self.hooks_run.set(self.hooks_run.get() + 1);
                hook();
            }
        }
    }

    impl CandidatePanel for HookPanel {
        fn show(&self) -> bool {
            self.fire();
            true
        }
        fn hide(&self) {
            self.hidden.set(self.hidden.get() + 1);
        }
        fn move_highlight(&self, _delta: isize) {
            self.fire();
        }
    }

    type RecController = Controller<RecClient, HookPanel, RecEnv>;

    fn rec_fixture() -> (Rc<RecController>, RecClient, HookPanel, RecEnv) {
        let (client, panel, env) = (RecClient::default(), HookPanel::default(), RecEnv::default());
        let controller = Rc::new(Controller::new(7, new_session(ConversionMode::Space, 0), panel.clone(), env.clone()));
        (controller, client, panel, env)
    }

    fn rec_type(c: &RecController, client: &RecClient, letters: &str) {
        for ch in letters.chars() {
            assert!(c.on_key(Route::Keys(vec![(Key::Letter { upper: false }, ch.to_string())]), client.clone()));
        }
    }

    fn rec_press(c: &RecController, client: &RecClient, key: Key, text: &str) -> bool {
        c.on_key(Route::Keys(vec![(key, text.to_string())]), client.clone())
    }

    #[test]
    fn reset_after_panic_clears_the_screen_and_keeps_the_conversion_mode() {
        let (c, client, panel, env) = rec_fixture();
        rec_type(&c, &client, "kyou");
        rec_press(&c, &client, Key::Space, " ");
        rec_press(&c, &client, Key::Space, " ");
        c.on_toggle_conversion_mode();
        c.syncing.set(true);
        c.panel_line.set(3);
        let (hidden, cancels) = (panel.hidden.get(), env.cancels.get());

        c.reset_after_panic();

        assert_eq!(*client.marked.borrow(), "");
        assert_eq!(panel.hidden.get(), hidden + 1);
        assert_eq!(c.mode(), ConversionMode::Live);
        assert_eq!(c.phase(), Phase::Empty);
        assert_eq!(env.cancels.get(), cancels + 1);
        assert!(!c.syncing.get());
        assert_eq!(c.panel_line.get(), 0);
    }

    #[test]
    fn reset_after_panic_logs_and_keeps_the_session_when_it_is_borrowed() {
        let (c, client, _panel, env) = rec_fixture();
        rec_type(&c, &client, "a");
        let _held = c.session.borrow_mut();
        c.reset_after_panic();
        assert_eq!(env.logs.borrow().len(), 1);
        assert_eq!(env.cancels.get(), 1);
    }

    #[test]
    fn a_stale_rerank_result_is_not_applied_after_the_session_is_recreated() {
        let (c, client, _panel, env) = rec_fixture();
        env.allowed.set(true);
        client.set_bundle(Some("com.apple.TextEdit"));
        rec_type(&c, &client, "kyou");
        let old = *env.sent.borrow().last().expect("a rerank request was sent");

        c.reset_after_panic();
        env.sent.borrow_mut().clear();
        rec_type(&c, &client, "kyou");
        let fresh = *env.sent.borrow().last().expect("a rerank request was sent");
        let before = client.marked.borrow().clone();

        assert_ne!(old, fresh, "generation numbers must not be reused");
        c.on_rerank_result(old, 1);
        assert_eq!(*client.marked.borrow(), before);
    }

    #[test]
    fn skipped_rerank_cancels_the_pending_request_and_sends_nothing() {
        let (c, client, _panel, env) = rec_fixture();
        env.allowed.set(true);
        client.set_bundle(None);
        rec_type(&c, &client, "kyou");
        assert!(env.sent.borrow().is_empty());
        assert!(env.cancels.get() > 0);
    }

    #[test]
    fn the_same_gate_reason_is_logged_once() {
        let (c, client, _panel, env) = rec_fixture();
        env.allowed.set(true);
        client.set_bundle(None);
        rec_type(&c, &client, "kyouha");
        assert_eq!(env.logs.borrow().len(), 1, "{:?}", env.logs.borrow());
    }

    #[test]
    fn bundle_id_is_retried_until_found_and_then_cached() {
        let (c, client, _panel, _env) = rec_fixture();
        rec_type(&c, &client, "a");
        let missed = client.bundle_calls.get();
        assert!(missed > 0);
        rec_type(&c, &client, "a");
        assert!(client.bundle_calls.get() > missed, "None is not cached");

        client.set_bundle(Some("com.apple.TextEdit"));
        rec_type(&c, &client, "a");
        let found = client.bundle_calls.get();
        client.set_bundle(Some("com.apple.Terminal"));
        rec_type(&c, &client, "a");
        assert_eq!(client.bundle_calls.get(), found, "a found id is not fetched again");
    }

    #[test]
    fn panel_callbacks_during_show_do_not_double_borrow_and_selection_changes_are_ignored() {
        let (c, client, panel, _env) = rec_fixture();
        rec_type(&c, &client, "kyou");
        rec_press(&c, &client, Key::Space, " ");
        let texts = c.candidate_texts();
        assert!(texts.len() >= 2);
        let weak = Rc::downgrade(&c);
        let other = texts[1].clone();
        *panel.hook.borrow_mut() = Some(Rc::new(move || {
            let c = weak.upgrade().expect("controller alive");
            assert!(c.is_syncing());
            assert!(!c.candidate_texts().is_empty());
            c.on_selection_changed(&other);
        }));

        rec_press(&c, &client, Key::Space, " ");

        assert!(panel.hooks_run.get() >= 2, "show and move_highlight both called back");
        assert!(!c.is_syncing());
        assert_eq!(c.phase(), Phase::Converting { selected: 1, window: true });
    }

    #[test]
    fn a_selection_change_outside_the_panel_update_moves_the_selection() {
        let (c, client, _panel, _env) = rec_fixture();
        rec_type(&c, &client, "kyou");
        rec_press(&c, &client, Key::Space, " ");
        rec_press(&c, &client, Key::Space, " ");
        let texts = c.candidate_texts();
        c.on_selection_changed(&texts[2]);
        assert!(matches!(c.phase(), Phase::Converting { selected: 2, .. }), "{:?}", c.phase());
        assert_eq!(c.panel_line.get(), 2);
    }

    #[test]
    fn flag_guard_clears_the_flag_on_drop_and_on_panic() {
        let flag = Cell::new(false);
        {
            let _guard = FlagGuard::set(&flag);
            assert!(flag.get());
        }
        assert!(!flag.get());
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = FlagGuard::set(&flag);
            panic!("boom");
        }));
        assert!(unwound.is_err());
        assert!(!flag.get());
    }
}
