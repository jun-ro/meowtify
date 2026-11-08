//! The Settings "Updates" row: look for a newer release, and install it when asked.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use meowtify_provider::app_update::{self, Release};

use slint::ComponentHandle;

use crate::MainWindow;

const IDLE: i32 = 0;
const BUSY: i32 = 1;
const READY: i32 = 2;

thread_local! {
    /// The toast function belongs to the window's thread, so async code reaches it from here.
    static TOAST: RefCell<Option<Rc<dyn Fn(&str)>>> = const { RefCell::new(None) };
}

/// Wires the two buttons and checks once at start. A failed start-up check says nothing,
/// because the user did not ask for it. A failed button press says why.
pub(crate) fn install(win: &MainWindow, rt: tokio::runtime::Handle, toast: Rc<dyn Fn(&str)>) {
    app_update::clean_up();
    TOAST.with(|t| *t.borrow_mut() = Some(toast));
    let found: Arc<Mutex<Option<Release>>> = Arc::default();
    let current = env!("CARGO_PKG_VERSION");

    let check = {
        let (weak, rt, found) = (win.as_weak(), rt.clone(), found.clone());
        Rc::new(move |quiet: bool| {
            let Some(w) = weak.upgrade() else { return };
            if w.get_update_state() == BUSY {
                return;
            }
            w.set_update_state(BUSY);
            if !quiet {
                w.set_update_info("Checking\u{2026}".into());
            }
            let (weak, found) = (weak.clone(), found.clone());
            rt.spawn(async move {
                let result = app_update::check(current).await;
                // `Rc` stays on the window's thread; only the answer crosses over.
                let _ = weak.upgrade_in_event_loop(move |w| match result {
                    Ok(Some(r)) => {
                        w.set_update_info(format!("Version {} is available.", r.version).into());
                        w.set_update_notes(r.notes.clone().into());
                        w.set_update_state(READY);
                        if quiet {
                            let text = format!("Meowtify {} is available. See Settings.", r.version);
                            TOAST.with(|t| if let Some(f) = t.borrow().as_ref() { f(&text) });
                        }
                        *found.lock().unwrap() = Some(r);
                    }
                    Ok(None) => {
                        w.set_update_info(if quiet { "".into() } else { format!("Meowtify {current} is the latest.").into() });
                        w.set_update_state(IDLE);
                    }
                    Err(e) => {
                        w.set_update_info(if quiet { "".into() } else { format!("Could not check: {e}").into() });
                        w.set_update_state(IDLE);
                    }
                });
            });
        })
    };

    {
        let check = check.clone();
        win.on_check_update(move || check(false));
    }
    {
        let (weak, rt, found) = (win.as_weak(), rt.clone(), found.clone());
        win.on_install_update(move || {
            let Some(w) = weak.upgrade() else { return };
            let Some(release) = found.lock().unwrap().clone() else { return };
            if w.get_update_state() == BUSY {
                return;
            }
            w.set_update_state(BUSY);
            w.set_update_info(format!("Downloading {}\u{2026}", release.version).into());
            let weak = weak.clone();
            rt.spawn(async move {
                let result = app_update::install(&release).await;
                let _ = weak.upgrade_in_event_loop(move |w| match result {
                    Ok(exe) => {
                        w.set_update_info("Restarting\u{2026}".into());
                        match std::process::Command::new(&exe).spawn() {
                            Ok(_) => {
                                let _ = slint::quit_event_loop();
                            }
                            Err(e) => {
                                w.set_update_info(format!("Updated. Start Meowtify again by hand ({e}).").into());
                                w.set_update_state(IDLE);
                            }
                        }
                    }
                    Err(e) => {
                        w.set_update_info(format!("Update failed: {e}").into());
                        w.set_update_state(READY);
                    }
                });
            });
        });
    }
    check(true);
}
