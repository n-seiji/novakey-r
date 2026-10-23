//! IMK のクライアント (IMKTextInput を実装するホストアプリ側オブジェクト) の安全なラッパー。
use objc2::rc::Id;
use objc2::runtime::AnyObject;
use objc2::{msg_send, msg_send_id};
use objc2_foundation::{NSNotFound, NSRange, NSString};

use crate::platform::{lossy_string, utf16_units};

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
            .and_then(|anchor| preceding_range(anchor, max_utf16))
            .and_then(|range| self.substring(range))
            .unwrap_or_default()
    }

    pub fn bundle_id(&self) -> Option<String> {
        // SAFETY: IMKTextInput の bundleIdentifier は NSString (nil 可) を返す。
        let id: Option<Id<NSString>> = unsafe { msg_send_id![&*self.0, bundleIdentifier] };
        id.map(|s| lossy_string(&s))
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
            let units = utf16_units(&*string?);
            Some(String::from_utf16_lossy(trim_split_pairs(&units)))
        }
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
}
