//! IMK のクライアント (IMKTextInput を実装するホストアプリ側オブジェクト) の安全なラッパー。
use objc2::rc::Id;
use objc2::runtime::AnyObject;
use objc2::{msg_send, msg_send_id};
use objc2_foundation::{NSNotFound, NSRange, NSString};

/// retain 済みのクライアント参照。Id が生存を保証するので、メソッドは安全に呼べる。
#[derive(Clone)]
pub struct Client(Id<AnyObject>);

const NOT_FOUND: usize = NSNotFound as usize;

impl Client {
    pub fn new(client: &AnyObject) -> Self {
        let ptr = client as *const AnyObject as *mut AnyObject;
        // SAFETY: ptr は有効な参照から作ったので非 null で生存している。retain して所有権を得る。
        let retained = unsafe { Id::retain(ptr) }.expect("参照から作ったポインタは非 null");
        Self(retained)
    }

    pub fn insert(&self, text: &str) {
        let ns = NSString::from_str(text);
        let replacement = NSRange { location: NOT_FOUND, length: 0 };
        // SAFETY: IMKTextInput の insertText:replacementRange: は (NSString, NSRange) を取り、戻り値なし。
        unsafe {
            let _: () = msg_send![&*self.0, insertText: &*ns, replacementRange: replacement];
        }
    }

    pub fn set_marked(&self, text: &str) {
        let ns = NSString::from_str(text);
        let selection = NSRange { location: ns.len_utf16(), length: 0 };
        let replacement = NSRange { location: NOT_FOUND, length: 0 };
        // SAFETY: IMKTextInput の setMarkedText:selectionRange:replacementRange: は (NSString, NSRange, NSRange) を取る。
        unsafe {
            let _: () = msg_send![
                &*self.0,
                setMarkedText: &*ns,
                selectionRange: selection,
                replacementRange: replacement
            ];
        }
    }

    /// マーク範囲 (なければ選択範囲) の直前のテキストを最大 max_utf16 UTF-16 単位返す。取れなければ空。
    pub fn preceding_text(&self, max_utf16: usize) -> String {
        self.anchor()
            .map(|anchor| (anchor.saturating_sub(max_utf16), anchor))
            .filter(|(start, anchor)| start < anchor)
            .and_then(|(start, anchor)| self.substring(NSRange { location: start, length: anchor - start }))
            .unwrap_or_default()
    }

    pub fn bundle_id(&self) -> Option<String> {
        // SAFETY: IMKTextInput の bundleIdentifier は NSString (nil 可) を返す。
        let id: Option<Id<NSString>> = unsafe { msg_send_id![&*self.0, bundleIdentifier] };
        id.map(|s| s.to_string())
    }

    fn anchor(&self) -> Option<usize> {
        // SAFETY: markedRange / selectedRange は引数なしで NSRange を返す。
        let (marked, selected): (NSRange, NSRange) =
            unsafe { (msg_send![&*self.0, markedRange], msg_send![&*self.0, selectedRange]) };
        [marked, selected]
            .into_iter()
            .map(|r| r.location)
            .find(|&location| location != NOT_FOUND)
    }

    fn substring(&self, range: NSRange) -> Option<String> {
        // SAFETY: attributedSubstringFromRange: は NSRange を取り NSAttributedString (nil 可) を返し、`string` は NSString を返す。
        unsafe {
            let attributed: Option<Id<AnyObject>> =
                msg_send_id![&*self.0, attributedSubstringFromRange: range];
            let string: Option<Id<NSString>> = msg_send_id![&*attributed?, string];
            string.map(|s| s.to_string())
        }
    }
}
