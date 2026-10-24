#![allow(unsafe_code)]
//! IMK のクライアント (IMKTextInput を実装するホストアプリ側オブジェクト) の安全なラッパー。
use objc2::rc::Id;
use objc2::runtime::AnyObject;
use objc2::sel;
use objc2_foundation::{NSNotFound, NSRange, NSString};

use crate::controller::TextClient;
use crate::platform::{catch_objc, lossy_string, send, utf16_units};

/// retain 済みのクライアント参照。Id が生存を保証するので、メソッドは安全に呼べる。
#[derive(Clone)]
pub struct Client(Id<AnyObject>);

const NOT_FOUND: usize = NSNotFound as usize;

impl TextClient for Client {
    fn insert(&self, text: &str) {
        let ns = NSString::from_str(text);
        let replacement = NSRange { location: NOT_FOUND, length: 0 };
        let action = sel!(insertText:replacementRange:);
        // SAFETY: IMKTextInput の insertText:replacementRange: は (NSString, NSRange) を取り、戻り値なし。
        catch_objc(|s| unsafe {
            send!(s, &*self.0, action; (&*ns => *const NSString, replacement => NSRange) -> ())
        });
    }

    fn set_marked(&self, text: &str) {
        let ns = NSString::from_str(text);
        let selection = NSRange { location: ns.len_utf16(), length: 0 };
        let replacement = NSRange { location: NOT_FOUND, length: 0 };
        let action = sel!(setMarkedText:selectionRange:replacementRange:);
        // SAFETY: IMKTextInput の setMarkedText:selectionRange:replacementRange: は (NSString, NSRange, NSRange) を取る。
        catch_objc(|s| unsafe {
            send!(
                s,
                &*self.0,
                action;
                (&*ns => *const NSString, selection => NSRange, replacement => NSRange) -> ()
            )
        });
    }

    // マーク範囲 (なければ選択範囲) の直前のテキストを最大 max_utf16 UTF-16 単位返す。取れなければ空。
    fn preceding_text(&self, max_utf16: usize) -> String {
        self.anchor()
            .and_then(|anchor| preceding_range(anchor, max_utf16))
            .and_then(|range| self.substring(range))
            .unwrap_or_default()
    }

    fn bundle_id(&self) -> Option<String> {
        let action = sel!(bundleIdentifier);
        // SAFETY: IMKTextInput の bundleIdentifier は NSString (nil 可) を返す。
        let id = catch_objc(|s| unsafe { send!(s, &*self.0, action; () -> *mut AnyObject) })?;
        // SAFETY: bundleIdentifier は NSString (nil 可) を返す。+0 の戻り値を retain して所有する。
        let id: Option<Id<NSString>> = unsafe { Id::retain(id.cast()) };
        id.map(|s| lossy_string(&s))
    }
}

impl Client {
    pub fn new(client: &AnyObject) -> Self {
        let ptr = client as *const AnyObject as *mut AnyObject;
        // SAFETY: ptr は有効な参照から作ったので非 null で生存している。retain して所有権を得る。
        let retained = unsafe { Id::retain(ptr) }.expect("参照から作ったポインタは非 null");
        Self(retained)
    }

    fn anchor(&self) -> Option<usize> {
        let (marked_range, selected_range) = (sel!(markedRange), sel!(selectedRange));
        // SAFETY: markedRange / selectedRange は引数なしで NSRange を返す。
        let (marked, selected) = catch_objc(|s| unsafe {
            (
                send!(s, &*self.0, marked_range; () -> NSRange),
                send!(s, &*self.0, selected_range; () -> NSRange),
            )
        })?;
        [marked, selected]
            .into_iter()
            .map(|r| r.location)
            .find(|&location| location != NOT_FOUND)
    }

    fn substring(&self, range: NSRange) -> Option<String> {
        // SAFETY: attributedSubstringFromRange: は NSRange を取り NSAttributedString (nil 可) を返し、`string` は NSString (nil 可) を返す。
        // どちらも +0 の戻り値なので retain して所有する。変換は f の外で行う。
        let (substring, to_string) = (sel!(attributedSubstringFromRange:), sel!(string));
        let string = catch_objc(|s| unsafe {
            let attributed = send!(s, &*self.0, substring; (range => NSRange) -> *mut AnyObject);
            let attributed: Option<Id<AnyObject>> = Id::retain(attributed);
            let string = send!(s, &**attributed.as_ref()?, to_string; () -> *mut AnyObject);
            Id::retain(string.cast::<NSString>())
        })??;
        let units = utf16_units(&string);
        Some(String::from_utf16_lossy(trim_split_pairs(&units)))
    }
}

/// anchor の直前 max 単位の範囲。空なら None。
fn preceding_range(anchor: usize, max: usize) -> Option<NSRange> {
    let start = anchor.saturating_sub(max);
    (start < anchor).then_some(NSRange { location: start, length: anchor - start })
}

/// 範囲の端でサロゲートペアが切れていたら、その半端な 1 単位を落とす。
fn trim_split_pairs(units: &[u16]) -> &[u16] {
    let units = match units.first() {
        Some(0xDC00..=0xDFFF) => &units[1..],
        _ => units,
    };
    match units.last() {
        Some(0xD800..=0xDBFF) => &units[..units.len() - 1],
        _ => units,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_is_the_last_max_units_before_anchor() {
        let r = preceding_range(10, 4).unwrap();
        assert_eq!((r.location, r.length), (6, 4));
    }

    #[test]
    fn range_is_clamped_to_the_start_of_the_document() {
        let r = preceding_range(3, 10).unwrap();
        assert_eq!((r.location, r.length), (0, 3));
        let r = preceding_range(4, 4).unwrap();
        assert_eq!((r.location, r.length), (0, 4));
    }

    #[test]
    fn range_is_none_when_nothing_precedes_anchor() {
        assert!(preceding_range(0, 8).is_none());
        assert!(preceding_range(5, 0).is_none());
    }

    #[test]
    fn a_leading_low_surrogate_is_dropped() {
        assert_eq!(trim_split_pairs(&[0xDE00, 0x61, 0x62]), &[0x61, 0x62]);
    }

    #[test]
    fn a_trailing_high_surrogate_is_dropped() {
        assert_eq!(trim_split_pairs(&[0x61, 0xD83D]), &[0x61]);
    }

    #[test]
    fn whole_pairs_and_empty_input_are_kept() {
        assert_eq!(trim_split_pairs(&[0xD83D, 0xDE00]), &[0xD83D, 0xDE00]);
        assert_eq!(trim_split_pairs(&[]), &[] as &[u16]);
    }

    use std::cell::{Cell, RefCell};

    use objc2::rc::{Allocated, Id};
    use objc2::{declare_class, msg_send_id, mutability, ClassType, DeclaredClass};
    use objc2::declare::ClassBuilder;
    use objc2::runtime::{AnyClass, Sel};
    use objc2::sel;
    use objc2_foundation::NSObject;

    /// 受け取った呼び出し。範囲は (location, length)。
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        Insert(String, (usize, usize)),
        SetMarked(String, (usize, usize), (usize, usize)),
    }

    struct FakeState {
        calls: RefCell<Vec<Call>>,
        bundle: Option<&'static str>,
        marked: (usize, usize),
        selected: (usize, usize),
        document: Vec<u16>,
        requested: Cell<Option<(usize, usize)>>,
    }

    impl FakeState {
        fn new() -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                bundle: None,
                marked: (NOT_FOUND, 0),
                selected: (NOT_FOUND, 0),
                document: Vec::new(),
                requested: Cell::new(None),
            }
        }

        fn substring(&self, wanted: NSRange) -> Option<String> {
            let end = wanted.location.checked_add(wanted.length)?;
            Some(String::from_utf16_lossy(self.document.get(wanted.location..end)?))
        }

        fn document(mut self, text: &str) -> Self {
            self.document = text.encode_utf16().collect();
            self
        }
    }

    fn range(r: (usize, usize)) -> NSRange {
        NSRange { location: r.0, length: r.1 }
    }

    declare_class!(
        struct FakeAttributed;

        unsafe impl ClassType for FakeAttributed {
            type Super = NSObject;
            type Mutability = mutability::InteriorMutable;
            const NAME: &'static str = "NovakeyRFakeAttributedString";
        }

        impl DeclaredClass for FakeAttributed {
            type Ivars = Id<NSString>;
        }

        unsafe impl FakeAttributed {
            #[method_id(string)]
            fn string(&self) -> Id<NSString> {
                self.ivars().clone()
            }
        }
    );

    declare_class!(
        struct FakeTextInput;

        unsafe impl ClassType for FakeTextInput {
            type Super = NSObject;
            type Mutability = mutability::InteriorMutable;
            const NAME: &'static str = "NovakeyRFakeTextInput";
        }

        impl DeclaredClass for FakeTextInput {
            type Ivars = FakeState;
        }

        unsafe impl FakeTextInput {
            #[method(insertText:replacementRange:)]
            fn insert_text(&self, text: &NSString, replacement: NSRange) {
                let call = Call::Insert(lossy_string(text), (replacement.location, replacement.length));
                self.ivars().calls.borrow_mut().push(call);
            }

            #[method(setMarkedText:selectionRange:replacementRange:)]
            fn set_marked_text(&self, text: &NSString, selection: NSRange, replacement: NSRange) {
                let call = Call::SetMarked(
                    lossy_string(text),
                    (selection.location, selection.length),
                    (replacement.location, replacement.length),
                );
                self.ivars().calls.borrow_mut().push(call);
            }

            #[method_id(bundleIdentifier)]
            fn bundle_identifier(&self) -> Option<Id<NSString>> {
                self.ivars().bundle.map(NSString::from_str)
            }

            #[method(markedRange)]
            fn marked_range(&self) -> NSRange {
                range(self.ivars().marked)
            }

            #[method(selectedRange)]
            fn selected_range(&self) -> NSRange {
                range(self.ivars().selected)
            }

            #[method_id(attributedSubstringFromRange:)]
            fn attributed_substring(&self, wanted: NSRange) -> Option<Id<FakeAttributed>> {
                attributed_substring_of(self.ivars(), wanted)
            }
        }
    );

    fn attributed_substring_of(state: &FakeState, wanted: NSRange) -> Option<Id<FakeAttributed>> {
        state.requested.set(Some((wanted.location, wanted.length)));
        let string = NSString::from_str(&state.substring(wanted)?);
        let this = FakeAttributed::alloc().set_ivars(string);
        // SAFETY: NSObject の指定イニシャライザ init を、ivars を設定済みの割り当てに呼ぶ。
        unsafe { msg_send_id![super(this), init] }
    }

    fn make_fake(state: FakeState) -> (Id<FakeTextInput>, Client) {
        let this: Allocated<FakeTextInput> = FakeTextInput::alloc();
        let this = this.set_ivars(state);
        // SAFETY: NSObject の指定イニシャライザ init を、ivars を設定済みの割り当てに呼ぶ。
        let fake: Id<FakeTextInput> = unsafe { msg_send_id![super(this), init] };
        let client = Client::new(&fake);
        (fake, client)
    }

    #[test]
    fn insert_sends_the_text_with_no_replacement_range() {
        let (fake, client) = make_fake(FakeState::new());
        client.insert("強い");
        assert_eq!(*fake.ivars().calls.borrow(), [Call::Insert("強い".to_string(), (NOT_FOUND, 0))]);
    }

    #[test]
    fn set_marked_puts_the_caret_at_the_end_in_utf16_units() {
        let (fake, client) = make_fake(FakeState::new());
        client.set_marked("a😀");
        // "a" 1 単位 + 絵文字 2 単位
        assert_eq!(
            *fake.ivars().calls.borrow(),
            [Call::SetMarked("a😀".to_string(), (3, 0), (NOT_FOUND, 0))]
        );
    }

    #[test]
    fn set_marked_with_empty_text_clears_the_mark() {
        let (fake, client) = make_fake(FakeState::new());
        client.set_marked("");
        assert_eq!(*fake.ivars().calls.borrow(), [Call::SetMarked(String::new(), (0, 0), (NOT_FOUND, 0))]);
    }

    #[test]
    fn bundle_id_is_returned_when_the_host_has_one() {
        let (_fake, client) = make_fake(FakeState { bundle: Some("com.apple.TextEdit"), ..FakeState::new() });
        assert_eq!(client.bundle_id().as_deref(), Some("com.apple.TextEdit"));
    }

    #[test]
    fn bundle_id_is_none_when_the_host_has_none() {
        let (_fake, client) = make_fake(FakeState::new());
        assert_eq!(client.bundle_id(), None);
    }

    #[test]
    fn preceding_text_reads_before_the_marked_range() {
        let state = FakeState { marked: (4, 2), selected: (1, 0), ..FakeState::new() }.document("今日は天気がいい");
        let (fake, client) = make_fake(state);
        assert_eq!(client.preceding_text(200), "今日は天");
        assert_eq!(fake.ivars().requested.get(), Some((0, 4)));
    }

    #[test]
    fn preceding_text_falls_back_to_the_selection_when_nothing_is_marked() {
        let state = FakeState { selected: (3, 0), ..FakeState::new() }.document("今日は天気");
        let (_fake, client) = make_fake(state);
        assert_eq!(client.preceding_text(200), "今日は");
    }

    #[test]
    fn preceding_text_is_limited_to_the_last_max_units() {
        let state = FakeState { selected: (5, 0), ..FakeState::new() }.document("今日は天気");
        let (fake, client) = make_fake(state);
        assert_eq!(client.preceding_text(2), "天気");
        assert_eq!(fake.ivars().requested.get(), Some((3, 2)));
    }

    #[test]
    fn preceding_text_keeps_whole_surrogate_pairs() {
        let state = FakeState { selected: (3, 0), ..FakeState::new() }.document("a😀b");
        let (_fake, client) = make_fake(state);
        assert_eq!(client.preceding_text(200), "a😀");
    }

    #[test]
    fn preceding_text_is_empty_without_an_anchor_or_a_preceding_range() {
        let (fake, client) = make_fake(FakeState::new().document("abc"));
        assert_eq!(client.preceding_text(200), "");
        assert_eq!(fake.ivars().requested.get(), None);

        let (fake, client) = make_fake(FakeState { selected: (0, 0), ..FakeState::new() }.document("abc"));
        assert_eq!(client.preceding_text(200), "");
        assert_eq!(fake.ivars().requested.get(), None);
    }

    #[test]
    fn preceding_text_is_empty_when_the_host_returns_no_substring() {
        let (fake, client) = make_fake(FakeState { selected: (5, 0), ..FakeState::new() }.document("abc"));
        assert_eq!(client.preceding_text(200), "");
        assert_eq!(fake.ivars().requested.get(), Some((0, 5)));
    }

    use std::mem;

    type InsertFn = extern "C-unwind" fn(*mut AnyObject, Sel, *const NSString, NSRange);
    type SetMarkedFn = extern "C-unwind" fn(*mut AnyObject, Sel, *const NSString, NSRange, NSRange);
    type BundleFn = extern "C-unwind" fn(*mut AnyObject, Sel) -> *mut AnyObject;
    type RangeFn = extern "C-unwind" fn(*mut AnyObject, Sel) -> NSRange;

    fn raise() -> ! {
        // SAFETY: NSException のクラスメソッド exceptionWithName:reason:userInfo: は (NSString, NSString, nil 可) を取り NSException を返す。
        let exception: Id<AnyObject> = unsafe {
            msg_send_id![
                objc2::class!(NSException),
                exceptionWithName: &*NSString::from_str("NovakeyRTestException"),
                reason: &*NSString::from_str("secret user text"),
                userInfo: std::ptr::null::<AnyObject>()
            ]
        };
        let throw = crate::platform::symbol(c"objc_exception_throw").expect("objc_exception_throw が引ける");
        // SAFETY: objc_exception_throw(id) は戻らない。dlsym の番地を C-unwind の型で呼ぶので、巻き戻りが定義された動作になる
        // (objc2::exception::throw は "C" 宣言で、そこから巻き戻るのは未定義動作)。呼び出し元は C-unwind の IMP。
        unsafe {
            let throw: unsafe extern "C-unwind" fn(*mut AnyObject) -> ! = mem::transmute(throw.as_ptr());
            throw(Id::into_raw(exception).cast())
        }
    }

    extern "C-unwind" fn raise_insert(_: *mut AnyObject, _: Sel, _: *const NSString, _: NSRange) {
        raise()
    }
    extern "C-unwind" fn raise_set_marked(_: *mut AnyObject, _: Sel, _: *const NSString, _: NSRange, _: NSRange) {
        raise()
    }
    extern "C-unwind" fn raise_bundle(_: *mut AnyObject, _: Sel) -> *mut AnyObject {
        raise()
    }
    extern "C-unwind" fn raise_range(_: *mut AnyObject, _: Sel) -> NSRange {
        raise()
    }

    /// 全メソッドが NSException を投げるクライアント。Rust で宣言したメソッドは extern "C" で巻き戻せず abort するので、
    /// C-unwind の IMP を直接登録する。
    fn raising() -> Client {
        static CLASS: std::sync::OnceLock<&'static AnyClass> = std::sync::OnceLock::new();
        let class = CLASS.get_or_init(|| {
            let mut builder = ClassBuilder::new("NovakeyRRaisingTextInput", NSObject::class())
                .expect("クラス名が未登録");
            // SAFETY: 各 IMP の型は IMKTextInput のメソッドの型 (引数と戻り値) に一致する。objc2 0.5 は C-unwind の IMP を
            // feature 付きでしか受けないので "C" の型へ読み替えるが、関数自体は C-unwind でコンパイルされ巻き戻せる。
            unsafe {
                builder.add_method(
                    sel!(insertText:replacementRange:),
                    mem::transmute::<InsertFn, extern "C" fn(*mut AnyObject, Sel, *const NSString, NSRange)>(raise_insert),
                );
                builder.add_method(
                    sel!(setMarkedText:selectionRange:replacementRange:),
                    mem::transmute::<SetMarkedFn, extern "C" fn(*mut AnyObject, Sel, *const NSString, NSRange, NSRange)>(
                        raise_set_marked,
                    ),
                );
                builder.add_method(
                    sel!(bundleIdentifier),
                    mem::transmute::<BundleFn, extern "C" fn(*mut AnyObject, Sel) -> *mut AnyObject>(raise_bundle),
                );
                let range_fn = mem::transmute::<RangeFn, extern "C" fn(*mut AnyObject, Sel) -> NSRange>(raise_range);
                builder.add_method(sel!(markedRange), range_fn);
                builder.add_method(sel!(selectedRange), range_fn);
            }
            builder.register()
        });
        // SAFETY: 登録済みクラスの new は引数なしで新しいインスタンスを返す。
        let object: Id<AnyObject> = unsafe { msg_send_id![*class, new] };
        Client::new(&object)
    }

    /// attributedSubstringFromRange: が self を返し、その `string` が未実装セレクタ (forwarding が例外を投げる) のクライアント。
    /// attributed を保持したまま例外が巻き戻る形で、msg_send! 経由だと release で abort した。
    fn self_returning() -> Client {
        extern "C" fn attributed(this: *mut AnyObject, _: Sel, _: NSRange) -> *mut AnyObject {
            this
        }
        extern "C" fn range(_: *mut AnyObject, _: Sel) -> NSRange {
            NSRange { location: 5, length: 0 }
        }
        static CLASS: std::sync::OnceLock<&'static AnyClass> = std::sync::OnceLock::new();
        let class = CLASS.get_or_init(|| {
            let mut builder = ClassBuilder::new("NovakeyRSelfReturningTextInput", NSObject::class())
                .expect("クラス名が未登録");
            // SAFETY: IMP の型は各メソッドの型 (引数と戻り値) に一致する。
            unsafe {
                builder.add_method(sel!(attributedSubstringFromRange:), attributed as extern "C" fn(_, _, _) -> _);
                builder.add_method(sel!(markedRange), range as extern "C" fn(_, _) -> _);
                builder.add_method(sel!(selectedRange), range as extern "C" fn(_, _) -> _);
            }
            builder.register()
        });
        // SAFETY: 登録済みクラスの new は引数なしで新しいインスタンスを返す。
        let object: Id<AnyObject> = unsafe { msg_send_id![*class, new] };
        Client::new(&object)
    }

    #[test]
    fn preceding_text_is_empty_when_string_raises_while_the_attributed_string_is_held() {
        assert_eq!(self_returning().preceding_text(200), "");
    }

    #[test]
    fn insert_survives_a_host_exception() {
        raising().insert("強い");
    }

    #[test]
    fn set_marked_survives_a_host_exception() {
        raising().set_marked("きょう");
    }

    #[test]
    fn bundle_id_is_none_when_the_host_raises() {
        assert_eq!(raising().bundle_id(), None);
    }

    #[test]
    fn preceding_text_is_empty_when_the_host_raises() {
        assert_eq!(raising().preceding_text(200), "");
    }
}
