//! Cocoa / InputMethodKit への薄い境界。unsafe はこのモジュールと imk_client.rs に閉じ込める。
use std::cell::RefCell;

use objc2::rc::{autoreleasepool, Allocated, Id};
use objc2::runtime::{AnyObject, Bool};
use objc2::{class, extern_class, msg_send, msg_send_id, mutability, sel, ClassType};
use objc2_foundation::{NSBundle, NSObject, NSString};

#[link(name = "InputMethodKit", kind = "framework")]
extern "C" {}

#[link(name = "AppKit", kind = "framework")]
extern "C" {}

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

/// NSAttributedString (候補パネルが渡す引数) の文字列部分を取り出す。
pub fn attributed_text(attributed: &AnyObject) -> Option<String> {
    // SAFETY: IMK の候補コールバックは NSAttributedString を渡し、`string` は NSString を返す。
    let string: Option<Id<NSString>> = unsafe { msg_send_id![attributed, string] };
    string.map(|s| s.to_string())
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

fn make_panel(server: &IMKServer) -> Option<Id<AnyObject>> {
    // SAFETY: IMKCandidates の initWithServer:panelType: は IMKServer と NSUInteger を取る。
    unsafe {
        let alloc: Allocated<AnyObject> = msg_send_id![class!(IMKCandidates), alloc];
        msg_send_id![alloc, initWithServer: server, panelType: K_IMK_SINGLE_COLUMN_SCROLLING_PANEL]
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

/// 入力メソッドのメニュー。warning があれば無効状態の注意項目を追加する。
pub fn build_menu(mode_title: &str, warning: Option<&str>) -> Id<AnyObject> {
    let title = NSString::from_str(mode_title);
    let empty = NSString::from_str("");
    // SAFETY: NSMenu/NSMenuItem の標準的な生成と addItem:。各セレクタの引数型は宣言と一致し、
    // toggleConversionMode: は NovakeyRInputController が実装する。
    unsafe {
        let menu: Id<AnyObject> = msg_send_id![class!(NSMenu), new];
        let alloc: Allocated<AnyObject> = msg_send_id![class!(NSMenuItem), alloc];
        let item: Id<AnyObject> = msg_send_id![
            alloc,
            initWithTitle: &*title,
            action: sel!(toggleConversionMode:),
            keyEquivalent: &*empty
        ];
        let _: () = msg_send![&*menu, addItem: &*item];
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
