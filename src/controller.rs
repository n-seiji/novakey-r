#![forbid(unsafe_code)]

//! キー処理の結果 (Effect) を画面側へ反映する層。Cocoa には触れず、ホストアプリ・候補パネル・環境は trait の向こうにある。
//! 本番は imk.rs / imk_client.rs が実装し、simulate.rs と下のテストは fake を差す。
//!
//! 借用の規則: `session` の RefCell を借りたまま TextClient / CandidatePanel / Environment を呼ばない。
//! 本物のパネルは show / move_highlight の中から candidates: などを同期で呼び戻し、session を再借用する。

use std::cell::{Cell, OnceCell, RefCell};

use crate::composer::{Composer, Phase};
use crate::deps::{EnglishMemory, Lexicon, ProdLexicon, ProdMemory};
use crate::input::Route;
use crate::keymap::Key;
use crate::kkc::RustEngine;
use crate::outbound;
use crate::session::{ConversionMode, Effect, Gates, Outcome, Session};

/// ホストアプリのテキスト入力。
pub trait TextClient {
    fn insert(&self, text: &str);
    fn set_marked(&self, text: &str);
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
    /// ライブ変換が有効か。アプリ全体で 1 つの設定 (全コントローラが共有し、再起動後も残る)。
    fn live_conversion(&self) -> bool;
    fn set_live_conversion(&self, enabled: bool);
    /// 入力中 (Phase が Empty 以外) かどうか。変わりうる操作のあとに、現在の値で呼ぶ (何度呼んでもよい)。
    /// 本番は App Nap を止める活動の開始 / 終了に使う。
    fn set_composing(&self, _composing: bool) {}
    fn log(&self, line: &str);
    /// 名前と数値だけを渡すこと。
    fn trace(&self, entry: &str, fields: &str);
}

pub fn new_session(mode: ConversionMode) -> Session<RustEngine, ProdLexicon, ProdMemory> {
    Session::new(Composer::new(RustEngine::new(), ProdLexicon), ProdMemory, mode)
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

/// L / M の既定は本番の語彙と記憶。テストだけが fake に差し替える。
pub struct Controller<C, P, E, L: Lexicon = ProdLexicon, M: EnglishMemory = ProdMemory> {
    /// トレース用の通し番号 (コントローラの見分け)。
    id: u64,
    session: RefCell<Session<RustEngine, L, M>>,
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
}

impl<C: TextClient + Clone, P: CandidatePanel, E: Environment, L: Lexicon, M: EnglishMemory> Controller<C, P, E, L, M> {
    pub fn new(id: u64, session: Session<RustEngine, L, M>, panel: P, env: E) -> Self {
        Self {
            id,
            session: RefCell::new(session),
            panel,
            env,
            last_client: RefCell::new(None),
            syncing: Cell::new(false),
            panel_line: Cell::new(0),
            bundle_id: OnceCell::new(),
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

    /// 未確定ローマ字が無いときの、いま確定した場合の文字列。あれば None (表示と一致しなくてよい)。
    pub fn commit_preview(&self) -> Option<String> {
        let session = self.session.borrow();
        (!session.has_romaji()).then(|| session.commit_preview())
    }

    fn client(&self) -> Option<C> {
        self.last_client.borrow().clone()
    }

    /// キー 1 打。処理したら true (false ならホストアプリへ素通しする)。
    pub fn on_key(&self, route: Route, client: C) -> bool {
        let handled = self.on_key_inner(route, client);
        self.sync_activity();
        handled
    }

    /// 入力中かどうかを環境へ伝える (Empty に戻ったら終わらせる)。
    fn sync_activity(&self) {
        // panic の後始末 (reset_after_panic) からも呼ぶので、借用中なら (状態が分からないので) 触らない。
        if let Ok(session) = self.session.try_borrow() {
            self.env.set_composing(session.phase() != Phase::Empty);
        }
    }

    fn on_key_inner(&self, route: Route, client: C) -> bool {
        if route != Route::PassThrough {
            *self.last_client.borrow_mut() = Some(client.clone());
            self.sync_conversion_mode();
        }
        match route {
            // Cmd / Ctrl / Option のショートカット (貼り付け・取り消し・全選択など) は、文脈に触れない。
            Route::PassThrough => false,
            Route::Translit(form) => self.dispatch(Key::Translit(form), "", &client),
            Route::Keys(keys) => {
                let handled = keys.iter().fold(false, |handled, (key, text)| self.dispatch(*key, text, &client) || handled);
                if !handled {
                    keys.iter().for_each(|(key, text)| self.remember_pass_through(*key, text, &client));
                }
                handled
            }
            Route::Navigation => {
                let has_pending = self.session.borrow().has_pending();
                self.env.trace(
                    "handleEvent",
                    &format!("id={} mode={:?} key=Navigation phase={:?} handled={has_pending}", self.id, self.mode(), self.phase()),
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
        self.sync_activity();
    }

    /// パネルで候補が選ばれた。
    pub fn on_candidate_selected(&self, text: &str) {
        let Some(client) = self.client() else { return };
        self.env.trace("candidateSelected", &format!("phase={:?}", self.phase()));
        let gates = self.gates(Some(&client));
        let effects = self.session.borrow_mut().commit_selected(&gates, text);
        self.run(effects, Some(&client));
        self.sync_activity();
    }

    pub fn on_commit_composition(&self, client: Option<C>) {
        let gates = self.gates(client.as_ref());
        let effects = self.session.borrow_mut().commit_composition(&gates);
        self.run(effects, client.as_ref());
        self.sync_activity();
    }

    /// 全体の設定を反転して保存し、このコントローラにも反映する。ほかのコントローラは次のキーかメニューで追従する。
    pub fn on_toggle_conversion_mode(&self) {
        self.env.set_live_conversion(!self.env.live_conversion());
        self.sync_conversion_mode();
        self.env.trace("toggleConversionMode", &format!("id={} mode={:?}", self.id, self.mode()));
    }

    /// 全体の設定とこのコントローラのモードが食い違っていれば、設定に合わせる。
    pub fn sync_conversion_mode(&self) {
        let live = self.env.live_conversion();
        if (self.mode() == ConversionMode::Live) == live {
            return;
        }
        let effects = self.session.borrow_mut().set_live(live);
        if let Some(client) = self.client() {
            self.run(effects, Some(&client));
        }
        self.sync_activity();
    }

    /// 今この場面で許される外向きの動き。呼び出しごとに作り、状態として持ち越さない。
    fn gates(&self, client: Option<&C>) -> Gates {
        let bundle_id = self.bundle_id(client);
        let secure = self.env.secure_input();
        Gates {
            learning: outbound::learning_allowed(secure, bundle_id),
            direct: outbound::direct_input(secure, bundle_id),
        }
    }

    /// クライアントの bundle id (学習判定に使う)。最初に取れたものを使い回す。
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

    /// 文脈を覚えて使ってよいアプリの bundle id。取れない・学習が許されない・素通し (セキュア入力・パスワード管理アプリ) のときは None。
    fn context_app(&self, client: Option<&C>) -> Option<String> {
        let gates = self.gates(client);
        let allowed = gates.learning && !gates.direct;
        self.bundle_id(client).filter(|_| allowed).map(str::to_string)
    }

    /// このアプリで直前に打った文字列 (覚えていなければ空)。
    fn context(&self, client: Option<&C>) -> String {
        self.context_app(client).map_or_else(String::new, |app| {
            crate::context::with(|store| store.get(&app, std::time::Instant::now()).to_string())
        })
    }

    /// アプリに入った文字列を文脈に足す。
    fn remember(&self, text: &str, client: Option<&C>) {
        if let Some(app) = self.context_app(client) {
            crate::context::with(|store| store.record(&app, text, std::time::Instant::now()));
        }
    }

    /// 処理しなかったキーがアプリへ素通しになった。入る文字は文脈に足す (Enter は改行)。カーソルが動くキーでも文脈は捨てない。
    fn remember_pass_through(&self, key: Key, text: &str, client: &C) {
        let Some(app) = self.context_app(Some(client)) else { return };
        match key {
            Key::Enter => self.remember("\n", Some(client)),
            Key::Backspace => crate::context::with(|store| store.pop(&app)),
            Key::Up | Key::Down | Key::Control | Key::Translit(_) | Key::Escape => {}
            _ => self.remember(text, Some(client)),
        }
    }

    /// session に判断させ、返ってきた Effect を実行する。処理したら true。
    fn dispatch(&self, key: Key, text: &str, client: &C) -> bool {
        let gates = self.gates(Some(client));
        let context = self.context(Some(client));
        // session の借用はここで外れる。以降のポート呼び出しは composer を再借用してよい。
        let outcome = {
            let mut session = self.session.borrow_mut();
            session.set_context(&context);
            session.handle(&gates, key, text)
        };
        let Outcome { handled, action, phase, effects } = outcome;
        self.run(effects, Some(client));
        match action {
            None => self.env.trace(
                "handleEvent",
                &format!("id={} mode={:?} key={} phase={phase:?} action=Romaji handled={handled}", self.id, self.mode(), key.name()),
            ),
            Some(action) => self.env.trace(
                "handleEvent",
                &format!(
                    "id={} mode={:?} key={} phase={phase:?} action={} handled={handled} now={:?}",
                    self.id,
                    self.mode(),
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
                    self.remember(&text, client);
                }
                Effect::ShowCandidates { selected } => self.show_candidates(selected),
                Effect::HideCandidates => self.panel.hide(),
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

}

// session の作り直しは本番の語彙と記憶に決まっている。
impl<C: TextClient + Clone, P: CandidatePanel, E: Environment> Controller<C, P, E> {
    /// panic で session が中途半端かもしれないので、現在のモードで作り直して画面側も揃える。
    /// 呼び出し側 (imk.rs) が、これ自体の panic を止める。
    pub fn reset_after_panic(&self) {
        match self.session.try_borrow_mut() {
            Ok(mut session) => {
                // 作り直した session にも、メニューの表示と同じ現在のモードを引き継ぐ。
                *session = new_session(session.mode());
            }
            Err(_) => self.env.log("reset_after_panic: session is borrowed, not recreated"),
        }
        self.panel.hide();
        self.syncing.set(false);
        self.panel_line.set(0);
        if let Some(client) = self.client() {
            client.set_marked("");
        }
        self.sync_activity();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;
    use crate::deps::{FakeLexicon, FakeMemory};
    use crate::kkc::{learning::Learning, KkcCore};

    use crate::simulate::{FakeClient, FakeEnv, FakePanel};

    type Fixture = (Controller<FakeClient, FakePanel, FakeEnv>, FakeClient, FakePanel);

    fn fixture() -> Fixture {
        let panel = FakePanel::default();
        let controller = Controller::new(1, new_session(ConversionMode::Space), panel.clone(), FakeEnv::default());
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
    fn 入力中だけ活動を持ち_確定でもキャンセルでも空に戻ったら手放す() {
        let f = fixture();
        let last = |f: &Fixture| f.0.env.last_composing();
        assert_eq!(last(&f), None);
        type_letters(&f, "a");
        assert_eq!(last(&f), Some(true));
        assert!(press(&f, Key::Enter, "\r"));
        assert_eq!(last(&f), Some(false), "確定");
        type_letters(&f, "a");
        assert_eq!(last(&f), Some(true));
        f.0.on_commit_composition(Some(f.1.clone()));
        assert_eq!(last(&f), Some(false), "commitComposition (非アクティブ化)");
        type_letters(&f, "a");
        assert!(press(&f, Key::Escape, "\u{1b}"));
        assert_eq!(f.0.phase(), Phase::Empty);
        assert_eq!(last(&f), Some(false), "キャンセル");
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
    fn ライブ変換の切り替えは全体設定として保存される() {
        let f = fixture();
        assert!(!f.0.env.live_conversion());
        f.0.on_toggle_conversion_mode();
        assert!(f.0.env.live_conversion());
        f.0.on_toggle_conversion_mode();
        assert!(!f.0.env.live_conversion());
    }

    #[test]
    fn 別のコントローラで切り替えても_次のキーでライブ変換に追従する() {
        let a = fixture();
        let b = Controller::new(2, new_session(ConversionMode::Space), FakePanel::default(), a.0.env.clone());
        let client = FakeClient::default();
        b.on_key(Route::Navigation, client.clone());
        a.0.on_key(Route::Navigation, a.1.clone());

        a.0.on_toggle_conversion_mode();
        assert_eq!(b.mode(), ConversionMode::Space);
        b.on_key(Route::Navigation, client);

        assert_eq!(b.mode(), ConversionMode::Live);
    }

    #[test]
    fn 保存済みの設定がライブなら_スペース変換で作ったコントローラも最初のキーでライブになる() {
        let f = fixture();
        f.0.env.set_live_conversion(true);
        assert_eq!(f.0.mode(), ConversionMode::Space);
        f.0.on_key(Route::Navigation, f.1.clone());
        assert_eq!(f.0.mode(), ConversionMode::Live);
    }

    #[test]
    fn 設定が未保存なら_ライブ変換はオフのまま() {
        let f = fixture();
        f.0.on_key(Route::Navigation, f.1.clone());
        assert_eq!(f.0.mode(), ConversionMode::Space);
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
        inserted: Rc<RefCell<Vec<String>>>,
    }

    impl RecClient {
        fn set_bundle(&self, id: Option<&str>) {
            *self.bundle.borrow_mut() = id.map(str::to_string);
        }
    }

    impl TextClient for RecClient {
        fn insert(&self, text: &str) {
            self.marked.borrow_mut().clear();
            self.inserted.borrow_mut().push(text.to_string());
        }
        fn set_marked(&self, text: &str) {
            *self.marked.borrow_mut() = text.to_string();
        }
        fn bundle_id(&self) -> Option<String> {
            self.bundle_calls.set(self.bundle_calls.get() + 1);
            self.bundle.borrow().clone()
        }
    }

    #[derive(Clone, Default)]
    struct RecEnv {
        secure: Rc<Cell<bool>>,
        logs: Rc<RefCell<Vec<String>>>,
        live: Rc<Cell<bool>>,
    }

    impl Environment for RecEnv {
        fn secure_input(&self) -> bool {
            self.secure.get()
        }
        fn live_conversion(&self) -> bool {
            self.live.get()
        }
        fn set_live_conversion(&self, enabled: bool) {
            self.live.set(enabled);
        }
        fn log(&self, line: &str) {
            self.logs.borrow_mut().push(line.to_string());
        }
        fn trace(&self, _entry: &str, _fields: &str) {}
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
        let controller = Rc::new(Controller::new(1, new_session(ConversionMode::Space), panel.clone(), env.clone()));
        (controller, client, panel, env)
    }

    fn rec_type<L: Lexicon, M: EnglishMemory>(c: &Controller<RecClient, HookPanel, RecEnv, L, M>, client: &RecClient, letters: &str) {
        for ch in letters.chars() {
            assert!(c.on_key(Route::Keys(vec![(Key::Letter { upper: false }, ch.to_string())]), client.clone()));
        }
    }

    fn rec_press<L: Lexicon, M: EnglishMemory>(c: &Controller<RecClient, HookPanel, RecEnv, L, M>, client: &RecClient, key: Key, text: &str) -> bool {
        c.on_key(Route::Keys(vec![(key, text.to_string())]), client.clone())
    }

    type FakeDepsController = Controller<RecClient, HookPanel, RecEnv, FakeLexicon, FakeMemory>;

    /// 英語の記憶への書き込みを記録する session (本物の記憶ファイルには触れない)。
    fn recording_controller(env: RecEnv) -> (FakeDepsController, FakeMemory) {
        let lexicon = FakeLexicon::default();
        let memory = lexicon.memory();
        let composer = Composer::new(RustEngine::new(), lexicon);
        let session = Session::new(composer, memory.clone(), ConversionMode::Space);
        (Controller::new(1, session, HookPanel::default(), env), memory)
    }

    /// 大文字 1 字を打ち、`secure_before_enter` にしてから Enter で確定し、記憶に書かれた語を返す。
    /// (セキュア入力中は打鍵が素通しになるので、作文は先に済ませておく。)
    fn learned_after_commit(env: RecEnv, bundle: Option<&str>, secure_before_enter: bool) -> Vec<String> {
        let client = RecClient::default();
        client.set_bundle(bundle);
        let (c, memory) = recording_controller(env.clone());
        rec_press(&c, &client, Key::Letter { upper: true }, "G");
        env.secure.set(secure_before_enter);
        rec_press(&c, &client, Key::Enter, "\r");
        memory.learned()
    }

    #[test]
    fn a_commit_is_learned_when_the_bundle_id_is_unknown() {
        assert_eq!(learned_after_commit(RecEnv::default(), None, false), vec!["G".to_string()]);
    }

    #[test]
    fn a_commit_is_learned_in_an_ordinary_app() {
        assert_eq!(learned_after_commit(RecEnv::default(), Some("com.apple.TextEdit"), false).len(), 1);
    }

    #[test]
    fn a_commit_is_not_learned_under_secure_input() {
        assert!(learned_after_commit(RecEnv::default(), Some("com.apple.TextEdit"), true).is_empty());
    }

    #[test]
    fn a_commit_is_not_learned_in_a_password_manager() {
        assert!(learned_after_commit(RecEnv::default(), Some("com.1password.1password"), false).is_empty());
    }

    /// 直接入力のとき、いくつかのキーを打って「処理した」と返ったものを集める。
    fn handled_in_direct_mode(env: RecEnv, bundle: Option<&str>) -> (Vec<bool>, RecClient, FakeMemory) {
        let client = RecClient::default();
        client.set_bundle(bundle);
        let (c, memory) = recording_controller(env);
        let mut handled = Vec::new();
        for ch in "kyou".chars() {
            handled.push(rec_press(&c, &client, Key::Letter { upper: false }, &ch.to_string()));
        }
        for (key, text) in [(Key::Symbol, "!"), (Key::Mark, "["), (Key::Hyphen, "-"), (Key::Space, " "), (Key::Enter, "\r")] {
            handled.push(rec_press(&c, &client, key, text));
        }
        (handled, client, memory)
    }

    #[test]
    fn keys_pass_through_uncomposed_under_secure_input() {
        let env = RecEnv::default();
        env.secure.set(true);
        let (handled, client, memory) = handled_in_direct_mode(env, Some("com.apple.TextEdit"));
        assert!(handled.iter().all(|h| !h), "{handled:?}");
        assert_eq!(*client.marked.borrow(), "");
        assert!(client.inserted.borrow().is_empty());
        assert!(memory.learned().is_empty());
    }

    #[test]
    fn keys_pass_through_uncomposed_in_a_password_manager() {
        let (handled, client, _) = handled_in_direct_mode(RecEnv::default(), Some("com.1password.1password"));
        assert!(handled.iter().all(|h| !h), "{handled:?}");
        assert_eq!(*client.marked.borrow(), "");
        assert!(client.inserted.borrow().is_empty());
    }

    #[test]
    fn keys_are_composed_as_usual_in_an_ordinary_app() {
        let (handled, client, _) = handled_in_direct_mode(RecEnv::default(), Some("com.apple.TextEdit"));
        assert!(handled[..4].iter().all(|h| *h), "{handled:?}");
        assert!(!client.inserted.borrow().is_empty() || !client.marked.borrow().is_empty());
    }

    #[test]
    fn a_composition_in_progress_is_committed_unlearned_when_secure_input_turns_on() {
        let env = RecEnv::default();
        let client = RecClient::default();
        let (c, memory) = recording_controller(env.clone());
        rec_press(&c, &client, Key::Letter { upper: true }, "G");
        rec_type(&c, &client, "n");
        assert_ne!(*client.marked.borrow(), "");
        env.secure.set(true);
        assert!(!rec_press(&c, &client, Key::Letter { upper: false }, "a"), "次のキーは素通し");
        assert_eq!(*client.marked.borrow(), "");
        assert_eq!(client.inserted.borrow().len(), 1, "作文中の分が 1 回だけ確定される");
        assert!(memory.learned().is_empty());
        assert!(!rec_press(&c, &client, Key::Letter { upper: false }, "b"));
        assert_eq!(client.inserted.borrow().len(), 1);
    }

    #[test]
    fn pending_romaji_is_committed_when_secure_input_turns_on() {
        let env = RecEnv::default();
        let client = RecClient::default();
        let (c, _) = recording_controller(env.clone());
        rec_type(&c, &client, "n");
        env.secure.set(true);
        assert!(!rec_press(&c, &client, Key::Letter { upper: false }, "a"));
        assert_eq!(client.inserted.borrow().len(), 1);
        assert_eq!(*client.marked.borrow(), "");
    }

    #[test]
    fn commit_composition_respects_the_learning_gate() {
        let env = RecEnv::default();
        let client = RecClient::default();
        let (c, memory) = recording_controller(env.clone());
        rec_press(&c, &client, Key::Letter { upper: true }, "G");
        env.secure.set(true);
        c.on_commit_composition(Some(client.clone()));
        assert!(memory.learned().is_empty(), "commit-all under secure input");

        env.secure.set(false);
        rec_press(&c, &client, Key::Letter { upper: true }, "G");
        c.on_commit_composition(Some(client.clone()));
        assert_eq!(memory.learned().len(), 1, "commit-all learns when allowed");
    }

    /// 英語の記憶と engine の学習の両方を観測できる controller (どちらも本物のファイルには触れない)。
    fn observing_controller(env: RecEnv) -> (FakeDepsController, Rc<KkcCore>, FakeMemory) {
        let core = Rc::new(KkcCore::open_with_learning(Learning::in_memory()).unwrap());
        let lexicon = FakeLexicon::default();
        let memory = lexicon.memory();
        let composer = Composer::new(RustEngine::with_core(core.clone()), lexicon);
        let session = Session::new(composer, memory.clone(), ConversionMode::Space);
        (Controller::new(1, session, HookPanel::default(), env), core, memory)
    }

    fn engine_learned(core: &KkcCore) -> bool {
        !core.learning().borrow().is_empty()
    }

    /// "kanji" を打ち、候補ウィンドウを開く。
    fn open_candidate_window(c: &FakeDepsController, client: &RecClient) {
        rec_type(c, client, "kanji");
        rec_press(c, client, Key::Space, " ");
        rec_press(c, client, Key::Space, " ");
        assert!(matches!(c.phase(), Phase::Converting { window: true, .. }));
    }

    fn candidate_click_learns(secure: bool) -> bool {
        let env = RecEnv::default();
        let client = RecClient::default();
        let (c, core, _) = observing_controller(env.clone());
        open_candidate_window(&c, &client);
        env.secure.set(secure);
        c.on_candidate_selected("幹事");
        engine_learned(&core)
    }

    #[test]
    fn a_candidate_click_is_learned_when_allowed() {
        assert!(candidate_click_learns(false));
    }

    #[test]
    fn a_candidate_click_is_not_learned_under_secure_input() {
        assert!(!candidate_click_learns(true));
    }

    fn picking_by_digit_learns(secure: bool) -> bool {
        let env = RecEnv::default();
        let client = RecClient::default();
        let (c, core, _) = observing_controller(env.clone());
        open_candidate_window(&c, &client);
        env.secure.set(secure);
        // 通常は数字キーで選択・確定する。セキュア入力中は作文中の分を確定してから数字が素通しになる。
        assert_eq!(rec_press(&c, &client, Key::Digit(3), "3"), !secure);
        assert_eq!(c.phase(), Phase::Empty);
        engine_learned(&core)
    }

    #[test]
    fn picking_a_candidate_by_digit_is_learned_when_allowed() {
        assert!(picking_by_digit_learns(false));
    }

    #[test]
    fn picking_a_candidate_by_digit_is_not_learned_under_secure_input() {
        assert!(!picking_by_digit_learns(true));
    }

    fn english_commit_is_remembered(secure: bool) -> bool {
        let env = RecEnv::default();
        let client = RecClient::default();
        let (c, core, memory) = observing_controller(env.clone());
        rec_type(&c, &client, "git");
        env.secure.set(secure);
        rec_press(&c, &client, Key::Enter, "\r");
        assert!(!engine_learned(&core));
        memory.learned().iter().any(|w| w == "git")
    }

    #[test]
    fn an_english_commit_is_remembered_when_allowed() {
        assert!(english_commit_is_remembered(false));
    }

    #[test]
    fn an_english_commit_is_not_remembered_under_secure_input() {
        assert!(!english_commit_is_remembered(true));
    }

    #[test]
    fn reset_after_panic_clears_the_screen_and_keeps_the_conversion_mode() {
        let (c, client, panel, _env) = rec_fixture();
        rec_type(&c, &client, "kyou");
        rec_press(&c, &client, Key::Space, " ");
        rec_press(&c, &client, Key::Space, " ");
        c.on_toggle_conversion_mode();
        c.syncing.set(true);
        c.panel_line.set(3);
        let hidden = panel.hidden.get();

        c.reset_after_panic();

        assert_eq!(*client.marked.borrow(), "");
        assert_eq!(panel.hidden.get(), hidden + 1);
        assert_eq!(c.mode(), ConversionMode::Live);
        assert_eq!(c.phase(), Phase::Empty);
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

    /// bundle id を持つクライアントで確定した文字列が、そのアプリの文脈に入る。
    fn context_of(app: &str) -> String {
        crate::context::with(|store| store.get(app, std::time::Instant::now()).to_string())
    }

    fn type_hello_and_commit(f: &Fixture) {
        assert!(press(f, Key::Letter { upper: true }, "H"));
        type_letters(f, "ello");
        assert!(press(f, Key::Enter, "\r"));
        assert_eq!(f.1.committed(), "Hello");
    }

    #[test]
    fn 確定した文字列はそのアプリの文脈にだけ入る() {
        let f = fixture();
        f.1.set_bundle("app.one");
        type_hello_and_commit(&f);
        assert_eq!(context_of("app.one"), "Hello");
        assert_eq!(context_of("app.two"), "");
    }

    #[test]
    fn 学習が許されないアプリと_bundle_id_が取れないクライアントでは覚えない() {
        let f = fixture();
        f.1.set_bundle("com.1password.1password");
        // パスワード管理アプリでは素通しになり、何も入らない (覚える文字も無い)。
        assert!(!press(&f, Key::Letter { upper: true }, "H"));
        assert_eq!(context_of("com.1password.1password"), "");
        // bundle id が取れないクライアントでは、確定しても誰の文脈にも入らない。
        let g = fixture();
        type_hello_and_commit(&g);
        assert!(crate::context::with(|store| store.is_empty()), "何も記録されない");
    }

    #[test]
    fn 直接入力では文脈を覚えず_使わない() {
        let (c, client, _panel, env) = rec_fixture();
        client.set_bundle(Some("app.one"));
        assert!(rec_press(&c, &client, Key::Symbol, "@"));
        assert_eq!(context_of("app.one"), "@");
        env.secure.set(true);
        assert!(!rec_press(&c, &client, Key::Letter { upper: false }, "a"));
        assert!(!rec_press(&c, &client, Key::Backspace, "\u{7f}"));
        assert_eq!(context_of("app.one"), "@", "セキュア入力中は記録も削除もしない");
        assert_eq!(c.context(Some(&client)), "", "セキュア入力中は文脈を使わない");
        assert!(!c.on_key(Route::PassThrough, client.clone()));
        assert_eq!(context_of("app.one"), "@", "セキュア入力中は捨てもしない");
    }

    #[test]
    fn 直接入れた文字は覚え_バックスペースで消え_改行は切れ目として残る() {
        let f = fixture();
        f.1.set_bundle("app.one");
        assert!(press(&f, Key::Symbol, "@"));
        assert!(press(&f, Key::Symbol, "&"));
        assert_eq!(context_of("app.one"), "@&");
        assert!(!press(&f, Key::Backspace, "\u{7f}"));
        assert_eq!(context_of("app.one"), "@");
        assert!(!press(&f, Key::Enter, "\r"));
        assert_eq!(context_of("app.one"), "@\n");
        assert!(press(&f, Key::Symbol, "@"));
        assert_eq!(context_of("app.one"), "@\n@");
        assert!(!f.0.on_key(Route::Navigation, f.1.clone()));
        assert_eq!(context_of("app.one"), "@\n@", "ナビゲーションでは捨てない");
    }

    #[test]
    fn 修飾キー付きの素通しと_tab_と矢印は文脈を捨てない() {
        let f = fixture();
        f.1.set_bundle("app.one");
        assert!(press(&f, Key::Symbol, "@"));
        // Cmd+V / Cmd+Z / Cmd+Backspace / Cmd+A など (Cmd / Ctrl / Option が付くと Route::PassThrough)。
        assert!(!f.0.on_key(Route::PassThrough, f.1.clone()));
        assert_eq!(context_of("app.one"), "@");
        assert!(!press(&f, Key::Control, ""));
        assert!(!press(&f, Key::Up, ""));
        assert!(!press(&f, Key::Down, ""));
        assert_eq!(context_of("app.one"), "@");
    }

    #[test]
    fn commit_composition_のあとも文脈は残る() {
        let f = fixture();
        f.1.set_bundle("app.one");
        type_hello_and_commit(&f);
        type_letters(&f, "a");
        f.0.on_commit_composition(Some(f.1.clone()));
        assert_eq!(f.1.committed(), "Helloあ");
        assert_eq!(context_of("app.one"), "Helloあ");
    }

    #[test]
    fn 英語を確定したあとの_感嘆符は半角のまま入る() {
        let f = fixture();
        f.1.set_bundle("app.one");
        type_hello_and_commit(&f);
        assert!(press(&f, Key::Punct, "!"));
        assert_eq!(f.1.committed(), "Hello!");
        assert_eq!(f.1.marked(), "");
        assert_eq!(f.0.phase(), Phase::Empty);
    }

    #[test]
    fn 日本語を確定したあとと文脈が無いときの_感嘆符は全角の組成になる() {
        let f = fixture();
        f.1.set_bundle("app.one");
        assert!(press(&f, Key::Punct, "!"));
        assert_eq!(f.1.marked(), "！", "文脈なし");
        assert!(press(&f, Key::Escape, "\u{1b}"));
        type_letters(&f, "a");
        assert!(press(&f, Key::Enter, "\r"));
        assert!(press(&f, Key::Punct, "!"));
        assert_eq!(f.1.marked(), "！", "日本語のあと");
        assert_eq!(f.1.committed(), "あ");
    }

    #[test]
    fn 英語を確定したあとでも_かぎ括弧は日本語のまま() {
        let f = fixture();
        f.1.set_bundle("app.one");
        type_hello_and_commit(&f);
        assert!(press(&f, Key::Punct, "["));
        assert_eq!(f.1.marked(), "「");
    }

    #[test]
    fn 改行のあとの_感嘆符は前の行が英語でも全角の組成になる() {
        let f = fixture();
        f.1.set_bundle("app.one");
        type_hello_and_commit(&f);
        assert!(!press(&f, Key::Enter, "\r"));
        assert_eq!(context_of("app.one"), "Hello\n");
        assert!(press(&f, Key::Punct, "!"));
        assert_eq!(f.1.marked(), "！");
    }
}
