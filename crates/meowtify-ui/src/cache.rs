//! Cache size and "Clear cache" for the settings page. The caches are the cover files and the
//! lyrics answers; both can be fetched again. Autoplay audio is not listed: it is the working
//! set of the songs about to play, and it is wiped at every start.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use meowtify_cache::DiskCache;
use meowtify_ui_view::MainWindow;
use slint::ComponentHandle;

/// Total bytes and file count of the plain files directly in `dir`.
fn dir_usage(dir: &Path) -> (u64, usize) {
    let Ok(entries) = std::fs::read_dir(dir) else { return (0, 0) };
    entries
        .flatten()
        .filter_map(|e| e.metadata().ok().filter(|m| m.is_file()))
        .fold((0, 0), |(bytes, n), m| (bytes + m.len(), n + 1))
}

fn clear_dir(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        if e.metadata().is_ok_and(|m| m.is_file()) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// `1.4 MB`, `812 KB`, `0 B`.
fn human(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= KB * KB * KB {
        format!("{:.1} GB", b / (KB * KB * KB))
    } else if b >= KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

fn summary(disk: Option<&DiskCache>, lyrics: &Path) -> String {
    let (cover_bytes, covers) = disk.map_or((0, 0), |d| (d.used_bytes(), d.len()));
    let (lyric_bytes, songs) = dir_usage(lyrics);
    format!("{} \u{b7} {covers} covers, {songs} lyrics", human(cover_bytes + lyric_bytes))
}

/// Measures (or clears, then measures) off the UI thread and shows the result.
fn run(win: &MainWindow, rt: &tokio::runtime::Handle, disk: Option<Arc<DiskCache>>, lyrics: PathBuf, clear: bool) {
    if win.get_cache_busy() {
        return;
    }
    win.set_cache_busy(true);
    if clear {
        win.set_cache_info("Clearing\u{2026}".into());
    }
    let weak = win.as_weak();
    rt.spawn_blocking(move || {
        if clear {
            if let Some(d) = &disk {
                d.clear();
            }
            clear_dir(&lyrics);
        }
        let mut text = summary(disk.as_deref(), &lyrics);
        if clear {
            text = format!("Cleared. {text}");
        }
        let _ = weak.upgrade_in_event_loop(move |w| {
            w.set_cache_info(text.into());
            w.set_cache_busy(false);
        });
    });
}

pub(crate) fn wire(win: &MainWindow, rt: &tokio::runtime::Handle, disk: Option<Arc<DiskCache>>) {
    let lyrics = meowtify_library::config_dir().join("lyrics");
    for clear in [false, true] {
        let (weak, rt, disk, lyrics) = (win.as_weak(), rt.clone(), disk.clone(), lyrics.clone());
        let go = move || {
            if let Some(w) = weak.upgrade() {
                run(&w, &rt, disk.clone(), lyrics.clone(), clear);
            }
        };
        if clear {
            win.on_clear_cache(go);
        } else {
            win.on_refresh_cache(go);
        }
    }
}
