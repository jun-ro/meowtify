//! Development aids for taking screenshots and driving the window without clicking. They are
//! compiled into debug builds, and into release builds only with the `dev` feature, so the
//! shipped program does not read these environment variables.
//!
//! - `MEOWTIFY_VIEW=<n>` opens page `n`.
//! - `MEOWTIFY_DIALOG=confirm|name` opens that dialog.
//! - `MEOWTIFY_LYRICS=1` opens the lyrics panel.
//! - `MEOWTIFY_QUERY=<text>` runs a provider search.
//! - `MEOWTIFY_AUTOPLAY=<n>` plays result `n` as soon as results arrive.
//! - `MEOWTIFY_SNAPSHOT=<file.png>` saves a screenshot after `MEOWTIFY_SNAPSHOT_SECS` seconds
//!   (default 3) and exits.

use std::time::Duration;

use slint::ComponentHandle;

use crate::MainWindow;

/// Applies the variables that are set. The returned timer must live as long as the window runs.
pub(crate) fn install(win: &MainWindow) -> slint::Timer {
    if let Some(v) = std::env::var("MEOWTIFY_VIEW").ok().and_then(|v| v.parse::<i32>().ok()) {
        // After the startup logic, which may open Settings.
        let weak = win.as_weak();
        slint::Timer::single_shot(Duration::from_millis(300), move || {
            if let Some(w) = weak.upgrade() {
                w.set_view(v);
            }
        });
    }
    if let Ok(d) = std::env::var("MEOWTIFY_DIALOG") {
        let weak = win.as_weak();
        slint::Timer::single_shot(Duration::from_millis(500), move || {
            if let Some(w) = weak.upgrade() {
                match d.as_str() {
                    "confirm" => {
                        w.set_confirm_title("Delete \u{201c}Mr. Brightside\u{201d}?".into());
                        w.set_confirm_text("The file is deleted from your music folder and removed from your playlists. This cannot be undone.".into());
                        w.set_confirm_open(true);
                    }
                    _ => {
                        w.set_name_title("Edit playlist".into());
                        w.set_name_initial("Four covers".into());
                        w.set_name_has_cover(true);
                        w.set_name_open(true);
                    }
                }
            }
        });
    }
    if std::env::var_os("MEOWTIFY_LYRICS").is_some() {
        let weak = win.as_weak();
        slint::Timer::single_shot(Duration::from_millis(600), move || {
            if let Some(w) = weak.upgrade() {
                w.invoke_toggle_lyrics();
            }
        });
    }
    if let Ok(q) = std::env::var("MEOWTIFY_QUERY") {
        crate::get::GET.with(|g| {
            if let Some(g) = g.borrow().as_ref() {
                g.search(&q);
            }
        });
    }
    let timer = slint::Timer::default();
    if let Ok(path) = std::env::var("MEOWTIFY_SNAPSHOT") {
        let weak = win.as_weak();
        let secs: u64 = std::env::var("MEOWTIFY_SNAPSHOT_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(3);
        timer.start(slint::TimerMode::SingleShot, Duration::from_secs(secs), move || {
            if let Some(w) = weak.upgrade() {
                match w.window().take_snapshot() {
                    Ok(buf) => {
                        let file = std::fs::File::create(&path).expect("create snapshot file");
                        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), buf.width(), buf.height());
                        enc.set_color(png::ColorType::Rgba);
                        enc.set_depth(png::BitDepth::Eight);
                        enc.write_header().unwrap().write_image_data(buf.as_bytes()).unwrap();
                    }
                    Err(e) => eprintln!("snapshot failed: {e:?}"),
                }
            }
            let _ = slint::quit_event_loop();
        });
    }
    timer
}

/// The result `MEOWTIFY_AUTOPLAY` asks to play, if the variable is set.
pub(crate) fn autoplay_pick() -> Option<usize> {
    std::env::var("MEOWTIFY_AUTOPLAY").ok().and_then(|v| v.parse().ok())
}
