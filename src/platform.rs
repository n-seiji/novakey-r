#![allow(unsafe_code)]
//! Cocoa / InputMethodKit への薄い境界。unsafe はこのモジュールと imk_client.rs に閉じ込める。
use std::cell::RefCell;
use std::ptr::NonNull;
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock, PoisonError};

use objc2::rc::{autoreleasepool, Allocated, Id};
use objc2::runtime::{AnyObject, Bool, Sel};
use objc2::{class, extern_class, msg_send, msg_send_id, mutability, sel, ClassType};
use crate::input::{KeyInput, Modifiers};
use objc2_foundation::{NSBundle, NSObject, NSRange, NSString};

#[link(name = "InputMethodKit", kind = "framework")]
unsafe extern "C" {
    static IMKCandidatesSendServerKeyEventFirst: &'static NSString;
}

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn IsSecureEventInputEnabled() -> u8;
}

#[link(name = "Foundation", kind = "framework")]
unsafe extern "C" {
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
/// どのスレッドからでも呼べるよう autorelease pool を張る。
pub fn log(message: &str) {
    autoreleasepool(|_| {
        let fmt = NSString::from_str("%@");
        let arg = NSString::from_str(message);
        // SAFETY: fmt と arg は呼び出し中生存する NSString で、"%@" は object 引数をちょうど 1 つ消費する。
        unsafe {
            NSLog(
                &*fmt as *const NSString as *mut AnyObject,
                &*arg as *const NSString,
            )
        };
    });
}

unsafe extern "C" {
    /// libdispatch (libSystem) のメインキュー本体。`dispatch_get_main_queue()` はこの番地を返すマクロ。
    static _dispatch_main_q: std::ffi::c_void;
    fn dispatch_async_f(
        queue: *const std::ffi::c_void,
        context: *mut std::ffi::c_void,
        work: extern "C" fn(*mut std::ffi::c_void),
    );
}

/// `f` を実行し、panic したら `Err` にする。
fn guarded(f: fn()) -> std::thread::Result<()> {
    std::panic::catch_unwind(f)
}

/// panic の場所 (file:line) だけを NSLog に出す panic hook を入れる。標準の hook はペイロードを stderr に出し、
/// 文字境界の panic などはユーザーの文字列を引用してしまう。IME として起動する経路でだけ呼ぶ (開発用フラグは標準のままでよい)。
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let place = info.location().map_or_else(String::new, |l| format!("{}:{}", l.file(), l.line()));
        log(&format!("panic at {place}"));
    }));
}

/// IMK から呼ばれるコールバック本体を panic から守る。panic が extern "C" の境界を越えると IME ごと abort するので、
/// ここで止めて `fallback()` を返す。ペイロードにはユーザーの入力が入りうるので、ログには出さない
/// (標準の panic hook も payload を stderr に出すため、起動時に `install_panic_hook` で差し替える)。
pub fn guard<T>(fallback: impl FnOnce() -> T, f: impl FnOnce() -> T) -> T {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|_| {
        log("panic in IMK callback (ignored)");
        fallback()
    })
}

unsafe extern "C" {
    fn dlsym(handle: *mut std::ffi::c_void, symbol: *const std::ffi::c_char) -> *mut std::ffi::c_void;
}

/// dlsym で引いたシンボルの番地。見つからなければ None。
pub fn symbol(name: &std::ffi::CStr) -> Option<NonNull<std::ffi::c_void>> {
    const RTLD_DEFAULT: isize = -2;
    // SAFETY: RTLD_DEFAULT と NUL 終端の名前を渡す dlsym は常に呼べる。
    NonNull::new(unsafe { dlsym(RTLD_DEFAULT as *mut std::ffi::c_void, name.as_ptr()) })
}

/// `catch_objc` の中で ObjC メッセージを送るための票。例外が巻き戻れる `objc_msgSend` の番地を持つ。
/// `msg_send!` は objc_msgSend を `extern "C"` (巻き戻りなし = nounwind) の型で呼ぶので、例外が通ると release で未定義動作になる。
/// ここでは dlsym で引いた番地を `extern "C-unwind"` の関数ポインタとして呼ぶ (リンク済みの宣言を経由すると LLVM が
/// nounwind の宣言に寄せてしまう)。
#[derive(Clone, Copy)]
pub struct Sender(NonNull<std::ffi::c_void>);

impl Sender {
    pub fn address(self) -> *mut std::ffi::c_void {
        self.0.as_ptr()
    }
}

fn sender() -> Option<Sender> {
    static ADDRESS: OnceLock<Option<usize>> = OnceLock::new();
    ADDRESS
        .get_or_init(|| {
            let address = symbol(c"objc_msgSend").map(|p| p.as_ptr() as usize);
            if address.is_none() {
                log("objc_msgSend not found: ObjC calls to the host/panel are skipped");
            }
            address
        })
        .and_then(|a| NonNull::new(a as *mut std::ffi::c_void))
        .map(Sender)
}

/// `Sender` で型付きのメッセージ送信をする。`send!(s, 受け手, セレクタ; (引数 => 型, ...) -> 戻り値の型)`。
/// 戻り値が ObjC オブジェクトなら生ポインタなので、呼び出し側で retain する。unsafe な文脈で使うこと。
///
/// arm64 専用: 戻り値の型で objc_msgSend_stret / _fpret を選ばないので、16 バイトを超える構造体は返せない。
/// msg_send! と違って引数と戻り値の型エンコーディングを debug でも検証しない。型は呼び出し側の責任。
/// 受け手は `&T` (T: Message) だけを取る (`&Id<T>` はコンパイルエラー)。セレクタは `catch_objc` の外で作ること。
macro_rules! send {
    ($s:expr, $recv:expr, $sel:expr; ($($arg:expr => $ty:ty),*) -> $ret:ty) => {{
        let function: unsafe extern "C-unwind" fn(*mut objc2::runtime::AnyObject, objc2::runtime::Sel $(, $ty)*) -> $ret =
            std::mem::transmute($s.address());
        function($crate::platform::recv($recv), $sel $(, $arg)*)
    }};
}
pub(crate) use send;

/// `send!` の受け手を生ポインタにする。`&Id<T>` (ポインタのポインタ) を通さないために T: Message を要求する。
pub fn recv<T: objc2::Message + ?Sized>(receiver: &T) -> *mut AnyObject {
    receiver as *const T as *const AnyObject as *mut AnyObject
}

/// ホストアプリや候補パネルへの Objective-C 呼び出し `f` を実行し、NSException が投げられたら `None` にする。
/// 例外が Rust のフレームに届くと "Rust cannot catch foreign exceptions" で IME ごと abort するので、呼び出しごとに止める。
/// ログには例外の名前だけを、名前ごとに 1 度出す (reason にはホストのテキストが入りうるので読まない)。
///
/// `f` には生の送信 (`send!`) だけを置くこと。**`f` は panic してはならない**: panic は @catch(id) に捕まらず、
/// "C" 宣言の try_catch を越えて abort する。Vec の確保や文字列変換など panic しうる処理は `f` の外で行う。
/// 例外を捕まえる送信の仕組みが使えない (dlsym が失敗した) ときは、`f` を呼ばずに `None` を返す。
/// `sel!` も初回登録で panic しうるので、`f` に入る前に作っておく。残る panic の可能性は `Id::retain` の debug_assert
/// (retain が返すオブジェクトが不正なときだけ。release では起きない) で、許容する。
pub fn catch_objc<T>(f: impl FnOnce(Sender) -> T) -> Option<T> {
    catch_objc_with(true, f)
}

/// `report` が false なら、例外の名前を読まず、ログも出さない。
fn catch_objc_with<T>(report: bool, f: impl FnOnce(Sender) -> T) -> Option<T> {
    let send = sender()?;
    let mut result = None;
    let mut call = Some(|| result = Some(f(send)));
    let mut exception: *mut objc2::ffi::objc_object = std::ptr::null_mut();
    // 例外が通る関数は C-unwind でなければならない (extern "C" だと巻き戻しで abort する)。objc2 の catch は
    // C-unwind を nightly 専用の feature でしか使わないので、同じ objc-sys の try_catch を直接呼ぶ。
    let function = function_for(&call);
    // SAFETY: 例外が通る経路は次の 4 つで、どれも巻き戻しが定義された動作になる。
    // (1) f の中の送信は `Sender` 経由の `extern "C-unwind"` 関数ポインタ呼び出しで、objc_msgSend のフレームも巻き戻せる。
    // (2) trampoline は `extern "C-unwind"`。(3) try_catch は @try/@catch(id) で例外をここで止めるので、例外は
    // try_catch の "C" の境界を越えない。C-unwind と C は呼び出し規約が同じで、transmute で変わるのは巻き戻しの可否の型だけ。
    // (4) f は panic しない (上の doc)。panic だと @catch(id) を素通りして "C" の境界を越えてしまう。
    // context は call への有効な可変参照で呼び出し中生存する。返った例外は +1 retain 済みなので Id::from_raw が所有する。
    let caught = unsafe {
        let function = std::mem::transmute::<extern "C-unwind" fn(*mut std::ffi::c_void), extern "C" fn(*mut std::ffi::c_void)>(function);
        objc2::ffi::try_catch(function, (&raw mut call).cast(), &raw mut exception)
    };
    drop(call);
    if caught == 0 {
        return result;
    }
    if !report {
        return None;
    }
    // SAFETY: 例外は有効な ObjC オブジェクトか NULL で、try_catch が +1 retain している。
    let exception: Option<Id<AnyObject>> = unsafe { Id::from_raw(exception.cast()) };
    let name = exception.as_deref().and_then(exception_name).unwrap_or_else(|| "(unknown)".to_string());
    log_exception_once(&name);
    None
}

extern "C-unwind" fn trampoline<G: FnOnce()>(context: *mut std::ffi::c_void) {
    // SAFETY: context は catch_objc が渡している `&mut Option<G>` で、try_catch は 1 回しか呼ばない。
    let call = unsafe { &mut *context.cast::<Option<G>>() };
    if let Some(call) = call.take() {
        call();
    }
}

fn function_for<G: FnOnce()>(_: &Option<G>) -> extern "C-unwind" fn(*mut std::ffi::c_void) {
    trampoline::<G>
}

fn exception_name(exception: &AnyObject) -> Option<String> {
    // NSException 以外が @throw されることもある。送信すると、その送信がまた投げて abort しうるので、
    // 先にクラスの継承を辿って NSException のサブクラスか確かめる (これはメッセージ送信ではない)。
    let target = class!(NSException);
    let mut class = Some(exception.class());
    let mut is_exception = false;
    // 継承は浅いが、壊れたクラス階層で止まらないように段数を切る。
    for _ in 0..64 {
        let Some(c) = class else { break };
        if std::ptr::eq(c, target) {
            is_exception = true;
            break;
        }
        class = c.superclass();
    }
    if !is_exception {
        return None;
    }
    let name_selector = sel!(name);
    // SAFETY: NSException の name は NSString を返す (+0)。送信は report なしの catch の中なので、投げられても再帰しない。
    let name = catch_objc_with(false, |s| unsafe { send!(s, exception, name_selector; () -> *mut AnyObject) })?;
    // SAFETY: name は NSString (nil 可) の +0 戻り値で、retain して所有する。
    let name: Option<Id<NSString>> = unsafe { Id::retain(name.cast()) };
    name.map(|n| lossy_string_quiet(&n))
}

const MAX_SEEN_EXCEPTIONS: usize = 32;

fn log_exception_once(name: &str) {
    static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let first = {
        let mut seen = SEEN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // 名前の種類が上限を超えたら、新しい名前は黙る (ログの洪水とメモリの増加を避ける)。
        let first = !seen.iter().any(|n| n == name) && seen.len() < MAX_SEEN_EXCEPTIONS;
        if first {
            seen.push(name.to_string());
        }
        first
    };
    if first {
        log(&format!("ObjC exception in host/panel call (ignored): {name}"));
    }
}

/// `f` をメインスレッドの run loop で 1 回実行する (どのスレッドからでも呼べる)。
pub fn run_on_main(f: fn()) {
    extern "C" fn trampoline(context: *mut std::ffi::c_void) {
        // SAFETY: run_on_main が `fn()` をそのままポインタ値として渡したもの。
        let f: fn() = unsafe { std::mem::transmute::<*mut std::ffi::c_void, fn()>(context) };
        // panic が extern "C" の境界を越えると IME ごと abort するので、ここで止めて続行する。
        // ペイロードにはユーザーの入力が入りうるので、ログには出さない。
        if guarded(f).is_err() {
            log("panic in main-queue callback (ignored)");
        }
    }
    // SAFETY: メインキューの番地は常に有効。context は fn ポインタ値で、trampoline が同じ型に戻す。
    unsafe { dispatch_async_f(&raw const _dispatch_main_q, f as *mut std::ffi::c_void, trampoline) };
}

type MainTask = Box<dyn FnOnce() + Send>;

/// `run_on_main` は `fn()` しか運べないので、データを持つ処理はここに積んで取り出し口 (`drain_main_tasks`) を投入する。
/// プロセスに 1 つだけのグローバルで、データを運べる投入口ができたら (`run_on_main_boxed`) なくす。
static MAIN_TASKS: Mutex<VecDeque<MainTask>> = Mutex::new(VecDeque::new());

/// `task` をメインスレッドの run loop で 1 回実行する (どのスレッドからでも呼べる)。投入した順に実行される。
pub fn post_to_main(task: MainTask) {
    MAIN_TASKS.lock().unwrap_or_else(PoisonError::into_inner).push_back(task);
    run_on_main(drain_main_tasks);
}

fn drain_main_tasks() {
    run_queued(&MAIN_TASKS);
}

/// 積まれた処理を順に実行する。1 件の panic で残りを止めない。
fn run_queued(queue: &Mutex<VecDeque<MainTask>>) {
    loop {
        let task = queue.lock().unwrap_or_else(PoisonError::into_inner).pop_front();
        let Some(task) = task else { return };
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(task)).is_err() {
            log("panic in main-queue task (ignored)");
        }
    }
}

pub fn is_secure_input_enabled() -> bool {
    // SAFETY: Carbon の引数なし C 関数で、状態を読むだけ。
    unsafe { IsSecureEventInputEnabled() != 0 }
}

/// NSString の UTF-16 単位列。`UTF8String` は単独サロゲートを含むと NULL を返し objc2 が確かめずに使うので通さない。
/// 送信が例外を投げたら空にする。
pub fn utf16_units(string: &NSString) -> Vec<u16> {
    utf16_units_with(string, true)
}

/// `report` が false なら、例外のログと名前の読み取りをしない (exception_name から呼ぶときの再帰を避ける)。
fn utf16_units_with(string: &NSString, report: bool) -> Vec<u16> {
    let (length, get_characters) = (sel!(length), sel!(getCharacters:range:));
    // SAFETY: length は NSUInteger を返す。
    let Some(length) = catch_objc_with(report, |s| unsafe { send!(s, string, length; () -> usize) }) else {
        return Vec::new();
    };
    let mut buffer: Vec<u16> = vec![0; length];
    let range = NSRange { location: 0, length };
    let out = buffer.as_mut_ptr();
    // SAFETY: getCharacters:range: は range が文字列内なら out に unichar を書き込む。
    // buffer は length 個の領域を確保してあり、範囲は 0..length。
    let filled = catch_objc_with(report, |s| unsafe {
        send!(s, string, get_characters; (out => *mut u16, range => NSRange) -> ())
    });
    if filled.is_none() {
        return Vec::new();
    }
    buffer
}

/// ホスト由来の NSString を String にする。不正なサロゲートは U+FFFD になる。
pub fn lossy_string(string: &NSString) -> String {
    String::from_utf16_lossy(&utf16_units(string))
}

fn lossy_string_quiet(string: &NSString) -> String {
    String::from_utf16_lossy(&utf16_units_with(string, false))
}

/// 候補パネルが渡す候補 (NSString か NSAttributedString) の文字列。どちらでもなければ None。
pub fn candidate_text(candidate: &AnyObject) -> Option<String> {
    // SAFETY: isKindOfClass: は Class を取り BOOL を返す。NSAttributedString の `string` は NSString を返し、
    // NSString と判定できたオブジェクトは NSString として参照してよい。
    unsafe {
        let attributed: Bool = msg_send![candidate, isKindOfClass: class!(NSAttributedString)];
        if attributed.as_bool() {
            let string: Option<Id<NSString>> = msg_send_id![candidate, string];
            return string.map(|s| lossy_string(&s));
        }
        let plain: Bool = msg_send![candidate, isKindOfClass: class!(NSString)];
        plain
            .as_bool()
            .then(|| lossy_string(&*(candidate as *const AnyObject as *const NSString)))
    }
}

const NS_EVENT_TYPE_KEY_DOWN: usize = 10;
#[cfg(test)]
const NS_EVENT_TYPE_KEY_UP: usize = 11;
pub const NS_EVENT_MASK_KEY_DOWN: usize = 1 << NS_EVENT_TYPE_KEY_DOWN;
const SHIFT_MODIFIER: usize = 1 << 17;
const CONTROL_MODIFIER: usize = 1 << 18;
const COMMAND_OR_OPTION_MODIFIERS: usize = (1 << 19) | (1 << 20);

/// NSEvent が keyDown なら中身を返す。
pub fn key_event(event: &AnyObject) -> Option<KeyInput> {
    // SAFETY: event は IMK が handleEvent:client: に渡す NSEvent。type / modifierFlags は NSUInteger、keyCode は
    // unsigned short を返し、characters と charactersIgnoringModifiers は keyDown でのみ呼ぶ (NSString、nil 可)。
    unsafe {
        let event_type: usize = msg_send![event, type];
        if event_type != NS_EVENT_TYPE_KEY_DOWN {
            return None;
        }
        let key_code: u16 = msg_send![event, keyCode];
        let modifiers: usize = msg_send![event, modifierFlags];
        let characters: Option<Id<NSString>> = msg_send_id![event, characters];
        let bare: Option<Id<NSString>> = msg_send_id![event, charactersIgnoringModifiers];
        Some(KeyInput {
            key_code,
            characters: characters.map(|s| lossy_string(&s)).unwrap_or_default(),
            characters_ignoring_modifiers: bare.map(|s| lossy_string(&s)).unwrap_or_default(),
            mods: Modifiers {
                shift: modifiers & SHIFT_MODIFIER != 0,
                control: modifiers & CONTROL_MODIFIER != 0,
                command_or_option: modifiers & COMMAND_OR_OPTION_MODIFIERS != 0,
            },
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

/// 候補パネルを (必要なら生成して) 表示する。非表示だったときだけ候補を読み直し、そのとき true を返す
/// (読み直したパネルは先頭行を選んでいる)。サーバー未接続なら何もせず false。
pub fn show_candidates() -> bool {
    CANDIDATES_PANEL.with(|cell| {
        if cell.borrow().is_none() {
            let panel = SERVER.with(|s| s.borrow().as_deref().and_then(make_panel));
            *cell.borrow_mut() = panel;
        }
        let Some(panel) = cell.borrow().clone() else {
            return false;
        };
        let (is_visible, update, show) = (sel!(isVisible), sel!(updateCandidates), sel!(show:));
        // SAFETY: panel は生成済みの IMKCandidates。isVisible は BOOL を返し、updateCandidates と show: は
        // 引数の型が一致する。updateCandidates は candidates: を同期で呼び戻すので、借用を外してから呼ぶ。
        catch_objc(|s| unsafe {
            let visible: Bool = send!(s, &*panel, is_visible; () -> Bool);
            if visible.as_bool() {
                return false;
            }
            send!(s, &*panel, update; () -> ());
            send!(s, &*panel, show; (K_IMK_LOCATE_CANDIDATES_BELOW_HINT => usize) -> ());
            true
        })
        .unwrap_or(false)
    })
}

pub fn hide_candidates() {
    CANDIDATES_PANEL.with(|cell| {
        if let Some(panel) = cell.borrow().as_deref() {
            let hide = sel!(hide);
            // SAFETY: panel は生成済みの IMKCandidates で、hide は引数なし。
            catch_objc(|s| unsafe { send!(s, panel, hide; () -> ()) });
        }
    });
}

/// 候補パネルのハイライトを delta 行動かす (正で下、負で上)。
/// candidateIdentifierAtLineNumber: は常に 0 を返して selectCandidateWithIdentifier: が効かないため、
/// パネルの moveDown: / moveUp: を使う。どちらも candidateSelectionChanged: を呼び戻しうるので、呼び出し側は借用を持たないこと。
pub fn move_candidate_highlight(delta: isize) {
    let Some(panel) = CANDIDATES_PANEL.with(|cell| cell.borrow().clone()) else {
        return;
    };
    let action = if delta > 0 { sel!(moveDown:) } else { sel!(moveUp:) };
    let perform = sel!(performSelector:withObject:);
    for _ in 0..delta.unsigned_abs() {
        // SAFETY: panel は生成済みの IMKCandidates (NSResponder) で、moveDown: / moveUp: は sender (nil 可) を取る。
        let moved = catch_objc(|s| unsafe {
            send!(
                s,
                &*panel,
                perform;
                (action => Sel, std::ptr::null::<AnyObject>() => *const AnyObject) -> *mut AnyObject
            );
        });
        if moved.is_none() {
            return;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_main_tasks_run_in_order_and_a_panic_does_not_stop_the_rest() {
        let queue = Mutex::new(VecDeque::new());
        let ran = std::sync::Arc::new(Mutex::new(Vec::new()));
        for n in 1..=3 {
            let ran = ran.clone();
            let task: MainTask = Box::new(move || {
                ran.lock().unwrap().push(n);
                if n == 2 {
                    panic!("task panic");
                }
            });
            queue.lock().unwrap().push_back(task);
        }
        run_queued(&queue);
        assert_eq!(*ran.lock().unwrap(), vec![1, 2, 3]);
        assert!(queue.lock().unwrap().is_empty());
    }

    fn ns_from_units(units: &[u16]) -> Id<NSString> {
        // SAFETY: stringWithCharacters:length: は unichar 配列とその長さを取り NSString を返す。
        unsafe { msg_send_id![class!(NSString), stringWithCharacters: units.as_ptr(), length: units.len()] }
    }

    #[test]
    fn lone_surrogates_become_replacement_characters() {
        let ns = ns_from_units(&[0x61, 0xDC00, 0x62, 0xD800]);
        assert_eq!(lossy_string(&ns), "a\u{FFFD}b\u{FFFD}");
    }

    #[test]
    fn valid_pairs_and_empty_strings_round_trip() {
        assert_eq!(lossy_string(&ns_from_units(&[0xD83D, 0xDE00, 0x3042])), "\u{1F600}あ");
        assert_eq!(lossy_string(&ns_from_units(&[])), "");
    }
    #[test]
    fn catch_objc_returns_the_value_when_nothing_is_thrown() {
        assert_eq!(catch_objc(|_| 7), Some(7));
    }

    #[test]
    fn catch_objc_turns_an_nsexception_into_none() {
        // SAFETY: NSArray の array は空配列を返し、objectAtIndex: は NSUInteger を取る。範囲外なので NSRangeException が投げられる。
        let array: Id<AnyObject> = unsafe { msg_send_id![class!(NSArray), array] };
        let object_at_index = sel!(objectAtIndex:);
        let caught = catch_objc(|s| unsafe {
            send!(s, &*array, object_at_index; (3usize => usize) -> *mut AnyObject);
        });
        assert_eq!(caught, None);
        assert_eq!(catch_objc(|_| "still works"), Some("still works"));
    }

    #[test]
    fn exception_name_reads_only_nsexception_subclasses() {
        // SAFETY: NSObject の new と NSException の exceptionWithName:reason:userInfo: は引数の型が一致する。
        let object: Id<AnyObject> = unsafe { msg_send_id![class!(NSObject), new] };
        assert_eq!(super::exception_name(&object), None);
        let name = NSString::from_str("NovakeyTestException");
        let exception: Id<AnyObject> = unsafe {
            msg_send_id![class!(NSException), exceptionWithName: &*name, reason: std::ptr::null::<AnyObject>(), userInfo: std::ptr::null::<AnyObject>()]
        };
        assert_eq!(super::exception_name(&exception).as_deref(), Some("NovakeyTestException"));
    }

    #[test]
    fn guard_returns_fallback_on_panic_and_value_otherwise() {
        assert_eq!(super::guard(|| 0, || 7), 7);
        assert_eq!(super::guard(|| 0, || -> i32 { panic!("boom") }), 0);
    }

    use super::guarded;

    #[test]
    fn a_panicking_callback_is_contained() {
        assert!(guarded(|| panic!("boom")).is_err());
        assert!(guarded(|| {}).is_ok());
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Point {
        x: f64,
        y: f64,
    }

    // SAFETY: CGPoint は f64 を 2 つ並べた構造体で、エンコーディングは {CGPoint=dd}。
    unsafe impl objc2::Encode for Point {
        const ENCODING: objc2::Encoding =
            objc2::Encoding::Struct("CGPoint", &[objc2::Encoding::Double, objc2::Encoding::Double]);
    }

    fn synth_event(event_type: usize, flags: usize, chars: &str, bare: &str, key_code: u16) -> Id<AnyObject> {
        let chars = NSString::from_str(chars);
        let bare = NSString::from_str(bare);
        let location = Point { x: 0.0, y: 0.0 };
        let context: Option<&AnyObject> = None;
        // SAFETY: keyEventWithType:... は NSEvent を返すクラスメソッド。引数の型は AppKit のヘッダどおり。
        unsafe {
            msg_send_id![
                class!(NSEvent),
                keyEventWithType: event_type,
                location: location,
                modifierFlags: flags,
                timestamp: 0.0f64,
                windowNumber: 0isize,
                context: context,
                characters: &*chars,
                charactersIgnoringModifiers: &*bare,
                isARepeat: Bool::NO,
                keyCode: key_code
            ]
        }
    }

    #[test]
    fn key_down_converts_characters_and_modifiers() {
        let ev = synth_event(NS_EVENT_TYPE_KEY_DOWN, SHIFT_MODIFIER | CONTROL_MODIFIER, "\u{b}", "k", 40);
        let input = key_event(&ev).expect("keyDown");
        assert_eq!(input.key_code, 40);
        assert_eq!(input.characters, "\u{b}");
        assert_eq!(input.characters_ignoring_modifiers, "k");
        assert_eq!(input.mods, Modifiers { shift: true, control: true, command_or_option: false });
    }

    #[test]
    fn command_and_option_both_map_to_command_or_option() {
        for flag in [1usize << 19, 1 << 20] {
            let ev = synth_event(NS_EVENT_TYPE_KEY_DOWN, flag, "a", "a", 0);
            assert_eq!(
                key_event(&ev).unwrap().mods,
                Modifiers { shift: false, control: false, command_or_option: true }
            );
        }
    }

    #[test]
    fn plain_key_has_no_modifiers() {
        let ev = synth_event(NS_EVENT_TYPE_KEY_DOWN, 0, "a", "a", 0);
        assert_eq!(key_event(&ev).unwrap().mods, Modifiers::default());
    }

    #[test]
    fn key_up_is_not_converted() {
        let ev = synth_event(NS_EVENT_TYPE_KEY_UP, 0, "a", "a", 0);
        assert!(key_event(&ev).is_none());
    }
}
