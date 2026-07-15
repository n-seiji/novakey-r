use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_foundation::{NSAutoreleasePool, NSBundle, NSString};

mod imk;
mod romaji_converter;

fn main() {
    imk::register_controller();

    unsafe {
        let _pool = NSAutoreleasePool::new();
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        let connection_name = NSString::from_str("me.sijis.inputmethod.NovakeyR_Connection");

        let bundle = NSBundle::mainBundle();
        let identifier = bundle.bundleIdentifier();

        if let Some(ref id) = identifier {
            imk::describe(id.as_ref() as *const NSString as *mut AnyObject);
        }
        imk::describe(connection_name.as_ref() as *const NSString as *mut AnyObject);

        imk::connect_imkserver(
            connection_name.as_ref() as *const NSString as *mut AnyObject,
            identifier
                .as_ref()
                .map(|id| id.as_ref() as *const NSString as *mut AnyObject)
                .unwrap_or(std::ptr::null_mut()),
        );

        let _: () = msg_send![app, run];
    }
}
