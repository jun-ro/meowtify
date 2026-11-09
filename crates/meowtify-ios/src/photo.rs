//! The playlist cover picker: the iPhone photo library, and the file that was chosen.
//!
//! `rfd` has no iOS backend, so the shared UI cannot open a desktop dialog here. This module
//! shows `UIImagePickerController` over the app's root view controller, then copies the chosen
//! image out of the picker's temporary file into the app's own temporary folder, so the shared
//! UI (`meowtify_ui::CoverPick`) can read it like any other file. It answers through that hook,
//! never by returning: the copy runs on a helper thread, so the UI thread is not blocked.
//!
//! Why not `PHPickerViewController`: `objc2-photos-ui` 0.3 only generates the macOS bindings for
//! it, so there is no iOS one to call. `UIImagePickerController` is in `objc2-ui-kit`.
//!
//! Cargo.toml additions:
//! ```toml
//! objc2-ui-kit = { version = "0.3", features = [
//!     "UIAlertController", "UIApplication", "UIResponder", "UIWindow", "UIViewController",
//!     "UINavigationController", "UIImagePickerController", "UIDocumentPickerViewController",
//!     "objc2-uniform-type-identifiers", "block2",
//! ] }
//! ```
//!
//! Info.plist key, required or the app is killed when the picker opens:
//! ```text
//! NSPhotoLibraryUsageDescription = Meowtify reads only the picture you pick, to use it as a
//!     playlist cover.
//! ```

// The picker is deprecated in favour of PHPicker, which the Rust bindings do not have for iOS.
#![allow(deprecated)]

use std::path::PathBuf;
use std::sync::Mutex;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, MainThreadMarker, MainThreadOnly};
use objc2_foundation::{NSDictionary, NSObject, NSObjectProtocol, NSString, NSURL};
use objc2_ui_kit::{
    UIApplication, UIImagePickerController, UIImagePickerControllerDelegate, UIImagePickerControllerImageURL,
    UIImagePickerControllerImageURLExportPreset, UIImagePickerControllerSourceType, UINavigationControllerDelegate,
};

use meowtify_ui::CoverPick;

/// Who to tell when the picker closes. Only one picker is open at a time.
static PENDING: Mutex<Option<Box<dyn FnOnce(CoverPick) + Send>>> = Mutex::new(None);

define_class!(
    /// Hears what the picker did. The picker keeps only a weak link to it, so it is kept alive
    /// by `pick`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "MeowtifyPhotoDelegate"]
    struct PhotoDelegate;

    unsafe impl NSObjectProtocol for PhotoDelegate {}

    // The picker is a navigation controller; these are all optional and unused.
    unsafe impl UINavigationControllerDelegate for PhotoDelegate {}

    unsafe impl UIImagePickerControllerDelegate for PhotoDelegate {
        #[unsafe(method(imagePickerController:didFinishPickingMediaWithInfo:))]
        fn did_pick(&self, picker: &UIImagePickerController, info: &NSDictionary<NSString, AnyObject>) {
            // The picker waits for its delegate to close it.
            picker.dismissViewControllerAnimated_completion(true, None);
            let url = unsafe { info.objectForKey(UIImagePickerControllerImageURL) };
            let path = url
                .and_then(|u| u.downcast::<NSURL>().ok())
                .and_then(|u| u.path())
                .map(|p| PathBuf::from(p.to_string()));
            let Some(path) = path else {
                finish(CoverPick::Failed("Could not read that picture".into()));
                return;
            };
            // The picker's file is temporary and may go away: take a copy the app owns.
            take_and_copy(path);
        }

        #[unsafe(method(imagePickerControllerDidCancel:))]
        fn was_cancelled(&self, picker: &UIImagePickerController) {
            picker.dismissViewControllerAnimated_completion(true, None);
            finish(CoverPick::Cancelled);
        }
    }
);

fn finish(answer: CoverPick) {
    let done = PENDING.lock().ok().and_then(|mut p| p.take());
    if let Some(done) = done {
        done(answer);
    }
}

/// Copies the picked file off the UI thread, then answers.
fn take_and_copy(source: PathBuf) {
    let done = PENDING.lock().ok().and_then(|mut p| p.take());
    let Some(done) = done else { return };
    std::thread::spawn(move || {
        let answer = match copy_to_temp(&source) {
            Ok(path) => CoverPick::Picked(path),
            Err(e) => CoverPick::Failed(e),
        };
        done(answer);
    });
}

/// A copy of `source` in the app's temporary folder. The shared UI only needs the contents;
/// the extension is kept so the name still says what it is.
fn copy_to_temp(source: &std::path::Path) -> Result<PathBuf, String> {
    let ext = source.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_else(|| "img".to_string());
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    let dir = std::env::temp_dir().join("meowtify-covers");
    std::fs::create_dir_all(&dir).map_err(|e| format!("Could not read that picture: {e}"))?;
    let dest = dir.join(format!("pick-{stamp}.{ext}"));
    std::fs::copy(source, &dest).map_err(|e| format!("Could not read that picture: {e}"))?;
    Ok(dest)
}

/// Shows the photo library. `done` gets the copied image, or why there is none. Call it on the
/// main thread.
pub fn pick(done: Box<dyn FnOnce(CoverPick) + Send>) {
    let Some(mtm) = MainThreadMarker::new() else {
        done(CoverPick::Failed("The photos open on the app's own thread only".into()));
        return;
    };
    let app = UIApplication::sharedApplication(mtm);
    let windows = app.windows();
    let root = windows.firstObject().and_then(|w| w.rootViewController());
    let Some(root) = root else {
        done(CoverPick::Failed("There is no window to show the photos in".into()));
        return;
    };
    // A picker is already open: drop the old request.
    if let Ok(mut pending) = PENDING.lock() {
        *pending = Some(done);
    }
    unsafe {
        let picker: Retained<UIImagePickerController> = msg_send![mtm.alloc::<UIImagePickerController>(), init];
        picker.setSourceType(UIImagePickerControllerSourceType::PhotoLibrary);
        // A HEIC photo is exported as a JPEG, which the shared image decoder reads.
        picker.setImageExportPreset(UIImagePickerControllerImageURLExportPreset::Compatible);
        let delegate: Retained<PhotoDelegate> = msg_send![mtm.alloc::<PhotoDelegate>(), init];
        let delegate: Retained<AnyObject> = Retained::cast_unchecked(delegate);
        picker.setDelegate(Some(&delegate));
        // The picker holds its delegate weakly, and the app asks for one picture at a time.
        std::mem::forget(delegate);
        root.presentViewController_animated_completion(&picker, true, None);
    }
}
