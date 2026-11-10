//! The Settings "Updates" row, the sidebar's update button, and the quiet re-check. A newer
//! release is looked for at start, then about every ten minutes without disturbing the user.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use meowtify_provider::app_update::{self, Release};

use slint::ComponentHandle;

use crate::MainWindow;

const IDLE: i32 = 0;
const BUSY: i32 = 1;
const READY: i32 = 2;

/// How often the quiet check's clock is read. The check itself waits [`app_update::wait_after`].
const TICK: Duration = Duration::from_secs(5);

thread_local! {
    /// The toast function belongs to the window's thread, so async code reaches it from here.
    static TOAST: RefCell<Option<Rc<dyn Fn(&str)>>> = const { RefCell::new(None) };
    /// When the last quiet check ran, and whether GitHub's rate limit refused it.
    static CLOCK: RefCell<Option<(Instant, bool)>> = const { RefCell::new(None) };
    /// The last release named in a toast, so each version is announced once.
    static ANNOUNCED: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// True the first time `version` is announced; later checks leave the toast alone.
fn announce(seen: &mut Option<String>, version: &str) -> bool {
    if seen.as_deref() == Some(version) {
        return false;
    }
    *seen = Some(version.to_string());
    true
}

/// A quiet check runs once the wait after the last one has passed: the ordinary interval, or
/// the longer pause after GitHub's rate limit.
fn due(now: Instant, last: Instant, rate_limited: bool) -> bool {
    now.duration_since(last) >= app_update::wait_after(rate_limited)
}

/// Notes that a quiet check is starting, which moves the next one a full wait away.
fn started(now: Instant) {
    CLOCK.with(|c| *c.borrow_mut() = Some((now, false)));
}

/// Notes whether the last quiet check was refused by the rate limit; its start time stays.
fn finished(rate_limited: bool) {
    CLOCK.with(|c| {
        let last = c.borrow().map(|(t, _)| t).unwrap_or_else(Instant::now);
        *c.borrow_mut() = Some((last, rate_limited));
    });
}

/// Wires the two Settings buttons, the sidebar's update button, and the quiet re-check. A failed
/// start-up check says nothing, because the user did not ask for it. A failed button press says
/// why. The returned timer must live as long as the window runs.
pub(crate) fn install(win: &MainWindow, rt: tokio::runtime::Handle, toast: Rc<dyn Fn(&str)>) -> slint::Timer {
    app_update::clean_up();
    TOAST.with(|t| *t.borrow_mut() = Some(toast));
    // The sidebar button only makes sense where a release has a program for the system.
    win.set_update_supported(cfg!(windows));
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
                let _ = weak.upgrade_in_event_loop(move |w| {
                    let limited = matches!(&result, Err(e) if app_update::rate_limited(e));
                    match result {
                        Ok(Some(r)) => {
                            w.set_update_info(format!("Version {} is available.", r.version).into());
                            w.set_update_notes(r.notes.clone().into());
                            w.set_update_state(READY);
                            w.set_update_installing(false);
                            w.set_update_progress(0.0);
                            if quiet && ANNOUNCED.with(|a| announce(&mut a.borrow_mut(), &r.version)) {
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
                    }
                    if quiet {
                        finished(limited);
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
            w.set_update_installing(true);
            w.set_update_progress(0.0);
            w.set_update_info(format!("Downloading {}\u{2026}", release.version).into());
            let weak = weak.clone();
            rt.spawn(async move {
                // The download reports the growing file; the icon turns that into a fraction.
                let progress = {
                    let weak = weak.clone();
                    move |done: u64, total: Option<u64>| {
                        let Some(total) = total.filter(|t| *t > 0) else { return };
                        let fraction = (done as f32 / total as f32).clamp(0.0, 1.0);
                        let _ = weak.upgrade_in_event_loop(move |w| w.set_update_progress(fraction));
                    }
                };
                let result = app_update::install_with_progress(&release, progress).await;
                let _ = weak.upgrade_in_event_loop(move |w| match result {
                    Ok(exe) => {
                        w.set_update_info("Restarting\u{2026}".into());
                        w.set_update_progress(1.0);
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
                        w.set_update_installing(false);
                        w.set_update_progress(0.0);
                        w.set_update_state(READY);
                    }
                });
            });
        });
    }

    // Quiet re-checks: never while a check or an install is running, and not while a release
    // already waits to be installed. The request is conditional, so an unchanged answer is free.
    let timer = slint::Timer::default();
    {
        let check = check.clone();
        let weak = win.as_weak();
        timer.start(slint::TimerMode::Repeated, TICK, move || {
            let Some(w) = weak.upgrade() else { return };
            // The browser sign-in of the downloader could not be read: say so once.
            if let Some(msg) = meowtify_provider::take_cookie_problem() {
                TOAST.with(|t| {
                    if let Some(f) = t.borrow().as_ref() {
                        f(&msg);
                    }
                });
            }
            if w.get_update_state() != IDLE {
                return;
            }
            let now = Instant::now();
            let (last, limited) = CLOCK.with(|c| c.borrow().map(|(t, r)| (t, r)).unwrap_or((now, false)));
            if !due(now, last, limited) {
                return;
            }
            started(now);
            check(true);
        });
    }
    started(Instant::now());
    check(true);
    timer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_is_announced_once() {
        let mut seen = None;
        assert!(announce(&mut seen, "0.2.0"));
        assert!(!announce(&mut seen, "0.2.0"), "the same version is not toasted twice");
        assert!(announce(&mut seen, "0.3.0"));
        assert!(!announce(&mut seen, "0.3.0"));
    }

    #[test]
    fn quiet_checks_wait_and_back_off() {
        let now = Instant::now();
        assert!(!due(now, now, false));
        assert!(!due(now + app_update::RECHECK_EVERY - Duration::from_secs(1), now, false));
        assert!(due(now + app_update::RECHECK_EVERY, now, false));
        assert!(!due(now + app_update::RECHECK_EVERY, now, true), "a rate limit waits longer");
        assert!(due(now + app_update::RATE_LIMIT_PAUSE, now, true));
    }
}
