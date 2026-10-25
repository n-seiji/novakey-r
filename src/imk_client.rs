#![allow(unsafe_code)]
//! IMK のクライアント (IMKTextInput を実装するホストアプリ側オブジェクト) の安全なラッパー。
use objc2::rc::Id;
use objc2::runtime::AnyObject;
use objc2::sel;
use objc2_foundation::{NSNotFound, NSRange, NSString};

use crate::controller::TextClient;
use crate::platform::{catch_objc, lossy_string, send};

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
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::RefCell;

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
    }

    impl FakeState {
        fn new() -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                bundle: None,
            }
        }
    }

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
        }
    );

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

    use std::mem;

    type InsertFn = extern "C-unwind" fn(*mut AnyObject, Sel, *const NSString, NSRange);
    type SetMarkedFn = extern "C-unwind" fn(*mut AnyObject, Sel, *const NSString, NSRange, NSRange);
    type BundleFn = extern "C-unwind" fn(*mut AnyObject, Sel) -> *mut AnyObject;

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
            }
            builder.register()
        });
        // SAFETY: 登録済みクラスの new は引数なしで新しいインスタンスを返す。
        let object: Id<AnyObject> = unsafe { msg_send_id![*class, new] };
        Client::new(&object)
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
}
