//! The song menu the iPhone draws itself: an action sheet from `UIAlertController`, so it looks
//! like the rest of iOS instead of the Slint popup made for a mouse.
//!
//! The rows come from `meowtify_ui` (the same labels, actions and red rows as the popup), and the
//! chosen one's place is reported back; `meowtify_ui` then dispatches it the usual way. This
//! module knows nothing about songs or playlists.

use std::sync::{Arc, Mutex};

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2_foundation::NSString;
use objc2_ui_kit::{UIAlertAction, UIAlertActionStyle, UIAlertController, UIAlertControllerStyle, UIApplication};
use std::ptr::NonNull;

use meowtify_ui::NativeRow;

type Choose = Box<dyn FnOnce(usize)>;

/// Shows the rows as an action sheet; `choose` gets the place of the row the user picked. Nothing
/// is called when the sheet is dismissed. Call it on the main thread.
pub fn present(rows: Vec<NativeRow>, choose: Choose) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let app = UIApplication::sharedApplication(mtm);
    #[allow(deprecated)]
    let windows = app.windows();
    let Some(root) = windows.firstObject().and_then(|w| w.rootViewController()) else { return };
    let alert = UIAlertController::alertControllerWithTitle_message_preferredStyle(None, None, UIAlertControllerStyle::ActionSheet, mtm);
    // The first row picked wins; the rest of the blocks find nothing left to call.
    let choose = Arc::new(Mutex::new(Some(choose)));
    for (i, row) in rows.iter().enumerate() {
        let shared = choose.clone();
        let handler = RcBlock::new(move |_: NonNull<UIAlertAction>| {
            let chosen = shared.lock().ok().and_then(|mut c| c.take());
            if let Some(chosen) = chosen {
                chosen(i);
            }
        });
        let style = if row.danger { UIAlertActionStyle::Destructive } else { UIAlertActionStyle::Default };
        let action =
            UIAlertAction::actionWithTitle_style_handler(Some(&NSString::from_str(&row.label)), style, Some(&handler), mtm);
        alert.addAction(&action);
    }
    // A way out that changes nothing.
    let handler = RcBlock::new(|_: NonNull<UIAlertAction>| {});
    let cancel = UIAlertAction::actionWithTitle_style_handler(Some(&NSString::from_str("Cancel")), UIAlertActionStyle::Cancel, Some(&handler), mtm);
    alert.addAction(&cancel);
    root.presentViewController_animated_completion(&alert, true, None);
}
