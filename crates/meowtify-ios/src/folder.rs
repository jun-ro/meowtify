//! The music folder picker. iOS shows its Files browser (`UIDocumentPickerViewController`); the
//! folder the user picks lives outside the app's own folder, so the app may only touch it while
//! it holds a security-scoped grant. A bookmark saved next to the settings brings the grant
//! back on the next launch.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, MainThreadMarker, MainThreadOnly};
use objc2_foundation::{NSArray, NSData, NSObject, NSObjectProtocol, NSURLBookmarkCreationOptions, NSURLBookmarkResolutionOptions, NSURL};
use objc2_ui_kit::{UIApplication, UIDocumentPickerDelegate, UIDocumentPickerViewController};
use objc2_uniform_type_identifiers::UTTypeFolder;

type Done = Box<dyn FnOnce(Option<PathBuf>) + Send>;

/// Who to tell when the picker closes. Only one picker is open at a time.
static PENDING: Mutex<Option<Done>> = Mutex::new(None);
/// Where the bookmark file goes.
static BOOKMARK: OnceLock<PathBuf> = OnceLock::new();

fn finish(path: Option<PathBuf>) {
    let done = PENDING.lock().ok().and_then(|mut p| p.take());
    if let Some(done) = done {
        done(path);
    }
}

define_class!(
    /// Hears what the picker did. The picker keeps only a weak link to it, so it is kept alive
    /// by `pick`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "MeowtifyFolderDelegate"]
    struct FolderDelegate;

    unsafe impl NSObjectProtocol for FolderDelegate {}

    unsafe impl UIDocumentPickerDelegate for FolderDelegate {
        #[unsafe(method(documentPicker:didPickDocumentsAtURLs:))]
        fn did_pick(&self, _picker: &UIDocumentPickerViewController, urls: &NSArray<NSURL>) {
            let picked = urls.firstObject().and_then(|url| grant(&url));
            finish(picked);
        }

        #[unsafe(method(documentPickerWasCancelled:))]
        fn was_cancelled(&self, _picker: &UIDocumentPickerViewController) {
            finish(None);
        }
    }
);

/// Takes the grant for the folder, keeps it until the app ends, and saves a bookmark so the next
/// launch can take it again. Returns the folder's path.
fn grant(url: &NSURL) -> Option<PathBuf> {
    unsafe {
        if !url.startAccessingSecurityScopedResource() {
            eprintln!("meowtify-ios: no access to the folder that was picked");
            return None;
        }
        if let (Some(file), Ok(data)) = (
            BOOKMARK.get(),
            url.bookmarkDataWithOptions_includingResourceValuesForKeys_relativeToURL_error(NSURLBookmarkCreationOptions(0), None, None),
        ) {
            if let Err(e) = std::fs::write(file, data.to_vec()) {
                eprintln!("meowtify-ios: cannot save the folder bookmark: {e}");
            }
        }
    }
    url.path().map(|p| PathBuf::from(p.to_string()))
}

/// Call once at start, with the folder that holds the settings. Returns the folder the user
/// picked in an earlier run, with its grant taken again, or `None` when there is none or iOS
/// no longer honours the bookmark.
pub fn restore(config_dir: &Path) -> Option<PathBuf> {
    let file = config_dir.join("library.bookmark");
    let _ = BOOKMARK.set(file.clone());
    let bytes = std::fs::read(&file).ok()?;
    let data = NSData::with_bytes(&bytes);
    let mut stale = objc2::runtime::Bool::NO;
    let url = unsafe { NSURL::URLByResolvingBookmarkData_options_relativeToURL_bookmarkDataIsStale_error(&data, NSURLBookmarkResolutionOptions(0), None, &mut stale) }.ok()?;
    let path = grant(&url);
    // A stale bookmark still worked this time; `grant` has saved a fresh one.
    path.filter(|p| p.is_dir())
}

/// Shows the Files browser for choosing a folder. `done` gets the path, or `None` when the user
/// cancelled. Call it on the main thread.
pub fn pick(done: Done) {
    let Some(mtm) = MainThreadMarker::new() else {
        done(None);
        return;
    };
    let app = UIApplication::sharedApplication(mtm);
    #[allow(deprecated)]
    let windows = app.windows();
    let root = windows.firstObject().and_then(|w| w.rootViewController());
    let Some(root) = root else {
        done(None);
        return;
    };
    // A picker is already open: drop the old request.
    if let Ok(mut pending) = PENDING.lock() {
        *pending = Some(done);
    }
    unsafe {
        let types = NSArray::from_slice(&[UTTypeFolder]);
        let picker = UIDocumentPickerViewController::initForOpeningContentTypes(mtm.alloc(), &types);
        let delegate: Retained<FolderDelegate> = msg_send![mtm.alloc::<FolderDelegate>(), init];
        picker.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        // The picker holds its delegate weakly, and the app asks for one folder at a time.
        std::mem::forget(delegate);
        root.presentViewController_animated_completion(&picker, true, None);
    }
}
