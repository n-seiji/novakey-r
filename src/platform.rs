//! Cocoa / InputMethodKit への薄い境界。unsafe はこのモジュールと imk_client.rs に閉じ込める。
use std::cell::RefCell;

use objc2::rc::{autoreleasepool, Allocated, Id};
use objc2::runtime::{AnyObject, Bool, Sel};
use objc2::{class, extern_class, msg_send, msg_send_id, mutability, sel, ClassType};
use objc2_foundation::{NSBundle, NSObject, NSString};

#[link(name = "InputMethodKit", kind = "framework")]
extern "C" {
    static IMKCandidatesSendServerKeyEventFirst: &'static NSString;
}

#[link(name = "AppKit", kind = "framework")]
extern "C" {}

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn IsSecureEventInputEnabled() -> bool;
}

#[link(name = "Foundation", kind = "framework")]
extern "C" {
    fn NSLog(fmt: *mut AnyObject, ...);
}

const K_IMK_SINGLE_COLUMN_SCROLLING_PANEL: usize = 1;
const K_IMK_LOCATE_CANDIDATES_BELOW_HINT: usize = 2;

extern_class!(
    #[derive(Debug, PartialEq, Eq, Hash)]
    pub struct IMKServer;

    unsafe impl ClassType for IMKServer {
        type Super = NSObject;
        type Mutability = mutability::InteriorMutable;
        const NAME: &'static str = "IMKServer";
    }
);

thread_local! {
    static SERVER: RefCell<Option<Id<IMKServer>>> = const { RefCell::new(None) };
    static CANDIDATES_PANEL: RefCell<Option<Id<AnyObject>>> = const { RefCell::new(None) };
}

/// NSLog に文字列を出す。書式文字列は固定の "%@" なので message 内の % は解釈されない。
pub fn log(message: &str) {
    let fmt = NSString::from_str("%@");
    let arg = NSString::from_str(message);
    // SAFETY: fmt と arg は呼び出し中生存する NSString で、"%@" は object 引数をちょうど 1 つ消費する。
    unsafe {
        NSLog(
            &*fmt as *const NSString as *mut AnyObject,
            &*arg as *const NSString,
        )
    };
}

pub fn is_secure_input_enabled() -> bool {
    // SAFETY: Carbon の引数なし C 関数で、状態を読むだけ。
    unsafe { IsSecureEventInputEnabled() }
}

/// 候補パネルが渡す候補 (NSString か NSAttributedString) の文字列。どちらでもなければ None。
pub fn candidate_text(candidate: &AnyObject) -> Option<String> {
    // SAFETY: isKindOfClass: は Class を取り BOOL を返す。NSAttributedString の `string` は NSString を返し、
    // NSString と判定できたオブジェクトは NSString として参照してよい。
    unsafe {
        let attributed: Bool = msg_send![candidate, isKindOfClass: class!(NSAttributedString)];
        if attributed.as_bool() {
            let string: Option<Id<NSString>> = msg_send_id![candidate, string];
            return string.map(|s| s.to_string());
        }
        let plain: Bool = msg_send![candidate, isKindOfClass: class!(NSString)];
        plain
            .as_bool()
            .then(|| (*(candidate as *const AnyObject as *const NSString)).to_string())
    }
}

const NS_EVENT_TYPE_KEY_DOWN: usize = 10;
pub const NS_EVENT_MASK_KEY_DOWN: usize = 1 << NS_EVENT_TYPE_KEY_DOWN;
const SHORTCUT_MODIFIERS: usize = (1 << 18) | (1 << 19) | (1 << 20);

/// keyDown の NSEvent から取り出した値。
pub struct KeyEvent {
    pub key_code: u16,
    pub characters: String,
    /// Command / Control / Option のいずれかが押されている。
    pub is_shortcut: bool,
}

/// NSEvent が keyDown なら中身を返す。
pub fn key_event(event: &AnyObject) -> Option<KeyEvent> {
    // SAFETY: event は IMK が handleEvent:client: に渡す NSEvent。type / modifierFlags は NSUInteger、keyCode は
    // unsigned short を返し、characters は keyDown でのみ呼ぶ (NSString、nil 可)。
    unsafe {
        let event_type: usize = msg_send![event, type];
        if event_type != NS_EVENT_TYPE_KEY_DOWN {
            return None;
        }
        let key_code: u16 = msg_send![event, keyCode];
        let modifiers: usize = msg_send![event, modifierFlags];
        let characters: Option<Id<NSString>> = msg_send_id![event, characters];
        Some(KeyEvent {
            key_code,
            characters: characters.map(|s| s.to_string()).unwrap_or_default(),
            is_shortcut: modifiers & SHORTCUT_MODIFIERS != 0,
        })
    }
}

/// 入力メソッドサーバーを起動し、メインの run loop に入る。戻らない想定。
pub fn run_application(connection_name: &str) {
    autoreleasepool(|_| {
        let connection_name = NSString::from_str(connection_name);
        // SAFETY: mainBundle は常に有効な NSBundle を返し、bundleIdentifier は引数なしの getter。
        let bundle_id = unsafe { NSBundle::mainBundle().bundleIdentifier() };
        if let Some(id) = &bundle_id {
            log(&format!("Object description: {}", id));
        }
        log(&format!("Object description: {}", connection_name));
        connect_imkserver(&connection_name, bundle_id.as_deref());
        // SAFETY: sharedApplication は NSApplication のシングルトンを返し、run は引数なしの run loop 開始。
        unsafe {
            let app: Id<AnyObject> = msg_send_id![class!(NSApplication), sharedApplication];
            let _: () = msg_send![&*app, run];
        }
    });
}

/// 現スレッドのメイン run loop を回す (スモークテスト用)。
pub fn run_main_run_loop() {
    // SAFETY: mainRunLoop は NSRunLoop を返し、run は引数なしで呼べる。
    unsafe {
        let run_loop: Id<AnyObject> = msg_send_id![class!(NSRunLoop), mainRunLoop];
        let _: () = msg_send![&*run_loop, run];
    }
}

fn connect_imkserver(name: &NSString, bundle_id: Option<&NSString>) {
    // SAFETY: initWithName:bundleIdentifier: は NSString と nil 可の NSString を取る IMKServer の指定イニシャライザ。
    let server: Option<Id<IMKServer>> = unsafe {
        msg_send_id![IMKServer::alloc(), initWithName: name, bundleIdentifier: bundle_id]
    };
    SERVER.with(|s| *s.borrow_mut() = server);
    log("IMKServer connection established");
}

/// 候補パネルを作る。キー入力は先に入力コントローラへ届くようにし、パネルは表示専用にする
/// (既定ではパネルが先にキーを受け取り、handleEvent:client: が呼ばれない)。
fn make_panel(server: &IMKServer) -> Option<Id<AnyObject>> {
    // SAFETY: IMKCandidates の initWithServer:panelType: は IMKServer と NSUInteger を取る。setAttributes: は
    // NSDictionary を取り、IMKCandidatesSendServerKeyEventFirst は InputMethodKit が公開する NSString 定数。
    unsafe {
        let alloc: Allocated<AnyObject> = msg_send_id![class!(IMKCandidates), alloc];
        let panel: Option<Id<AnyObject>> =
            msg_send_id![alloc, initWithServer: server, panelType: K_IMK_SINGLE_COLUMN_SCROLLING_PANEL];
        let panel = panel?;
        let yes: Id<AnyObject> = msg_send_id![class!(NSNumber), numberWithBool: Bool::YES];
        let attributes: Id<AnyObject> = msg_send_id![
            class!(NSDictionary),
            dictionaryWithObject: &*yes,
            forKey: IMKCandidatesSendServerKeyEventFirst
        ];
        let _: () = msg_send![&*panel, setAttributes: &*attributes];
        Some(panel)
    }
}

/// 候補パネルを (必要なら生成して) 更新・表示する。サーバー未接続なら何もしない。
pub fn show_candidates() {
    CANDIDATES_PANEL.with(|cell| {
        if cell.borrow().is_none() {
            let panel = SERVER.with(|s| s.borrow().as_deref().and_then(make_panel));
            *cell.borrow_mut() = panel;
        }
        if let Some(panel) = cell.borrow().as_deref() {
            // SAFETY: panel は生成済みの IMKCandidates で、両セレクタとも引数の型が一致する。
            unsafe {
                let _: () = msg_send![panel, updateCandidates];
                let _: () = msg_send![panel, show: K_IMK_LOCATE_CANDIDATES_BELOW_HINT];
            }
        }
    });
}

pub fn hide_candidates() {
    CANDIDATES_PANEL.with(|cell| {
        if let Some(panel) = cell.borrow().as_deref() {
            // SAFETY: panel は生成済みの IMKCandidates で、hide は引数なし。
            unsafe {
                let _: () = msg_send![panel, hide];
            }
        }
    });
}

/// 候補パネルのハイライトを line 行目 (0 始まり) に合わせる。パネル未生成なら何もしない。
/// selectCandidateWithIdentifier: は candidateSelectionChanged: を同期で呼び戻すので、呼び出し側は借用を持たないこと。
pub fn select_candidate(line: usize) {
    let line = line as isize;
    let panel = CANDIDATES_PANEL.with(|cell| cell.borrow().clone());
    if let Some(panel) = panel {
        // SAFETY: panel は生成済みの IMKCandidates。candidateIdentifierAtLineNumber: は NSInteger を取り NSInteger を返し、
        // selectCandidateWithIdentifier: は NSInteger を取り BOOL を返す。
        unsafe {
            let identifier: isize = msg_send![&*panel, candidateIdentifierAtLineNumber: line];
            let _: Bool = msg_send![&*panel, selectCandidateWithIdentifier: identifier];
        }
    }
}

/// NSUserDefaults の真偽値。未設定なら false。
pub fn defaults_bool(key: &str) -> bool {
    let key = NSString::from_str(key);
    // SAFETY: standardUserDefaults は NSUserDefaults を返し、boolForKey: は NSString を取り BOOL を返す。
    unsafe {
        let defaults: Id<AnyObject> = msg_send_id![class!(NSUserDefaults), standardUserDefaults];
        let value: Bool = msg_send![&*defaults, boolForKey: &*key];
        value.as_bool()
    }
}

pub fn set_defaults_bool(key: &str, value: bool) {
    let key = NSString::from_str(key);
    // SAFETY: setBool:forKey: は (BOOL, NSString) を取り戻り値なし。
    unsafe {
        let defaults: Id<AnyObject> = msg_send_id![class!(NSUserDefaults), standardUserDefaults];
        let _: () = msg_send![&*defaults, setBool: Bool::new(value), forKey: &*key];
    }
}

/// 入力メソッドのメニュー。items は (タイトル, アクション)。warning があれば無効状態の注意項目を追加する。
pub fn build_menu(items: &[(&str, Sel)], warning: Option<&str>) -> Id<AnyObject> {
    let empty = NSString::from_str("");
    // SAFETY: NSMenu/NSMenuItem の標準的な生成と addItem:。各セレクタの引数型は宣言と一致し、
    // アクションは NovakeyRInputController が実装する。
    unsafe {
        let menu: Id<AnyObject> = msg_send_id![class!(NSMenu), new];
        for (title, action) in items {
            let title = NSString::from_str(title);
            let alloc: Allocated<AnyObject> = msg_send_id![class!(NSMenuItem), alloc];
            let item: Id<AnyObject> = msg_send_id![
                alloc,
                initWithTitle: &*title,
                action: *action,
                keyEquivalent: &*empty
            ];
            let _: () = msg_send![&*menu, addItem: &*item];
        }
        if let Some(warning) = warning {
            let warn = NSString::from_str(warning);
            let warn_item: Id<AnyObject> = msg_send_id![class!(NSMenuItem), new];
            let _: () = msg_send![&*warn_item, setTitle: &*warn];
            let _: () = msg_send![&*warn_item, setEnabled: Bool::NO];
            let _: () = msg_send![&*menu, addItem: &*warn_item];
        }
        menu
    }
}
