//! Progress of a file that is still being written, shared between the code that
//! downloads it and the player that already plays it.
//!
//! The downloader calls [`register`] with the file path and keeps the [`Growth`] up to
//! date. The player asks [`lookup`] for the path it is about to open. A path with no
//! entry is an ordinary finished file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

const RUNNING: u8 = 0;
const DONE: u8 = 1;
const FAILED: u8 = 2;

#[derive(Debug, Default)]
pub struct Growth {
    written: AtomicU64,
    state: AtomicU8,
}

impl Growth {
    pub fn new() -> Arc<Growth> {
        Arc::new(Growth::default())
    }

    /// Bytes at the start of the file that are on disk.
    pub fn written(&self) -> u64 {
        self.written.load(Ordering::Acquire)
    }

    pub fn set_written(&self, n: u64) {
        self.written.store(n, Ordering::Release);
    }

    /// The whole file is on disk. Sets `written` to `total` first.
    pub fn finish(&self, total: u64) {
        self.set_written(total);
        self.state.store(DONE, Ordering::Release);
    }

    /// The download stopped and the file stays incomplete.
    pub fn fail(&self) {
        self.state.store(FAILED, Ordering::Release);
    }

    pub fn is_done(&self) -> bool {
        self.state.load(Ordering::Acquire) == DONE
    }

    pub fn is_failed(&self) -> bool {
        self.state.load(Ordering::Acquire) == FAILED
    }

    /// Done or failed: `written` will not grow any more.
    pub fn is_over(&self) -> bool {
        self.state.load(Ordering::Acquire) != RUNNING
    }
}

fn registry() -> &'static Mutex<HashMap<PathBuf, Arc<Growth>>> {
    static REG: OnceLock<Mutex<HashMap<PathBuf, Arc<Growth>>>> = OnceLock::new();
    REG.get_or_init(Default::default)
}

/// Announces that `path` is being written. A finished entry is kept until
/// [`forget`], so a caller that looks up late still learns the file is complete.
pub fn register(path: &Path, growth: Arc<Growth>) {
    registry().lock().unwrap().insert(path.to_path_buf(), growth);
}

/// The growth record for `path`, if it was registered and not forgotten.
pub fn lookup(path: &Path) -> Option<Arc<Growth>> {
    registry().lock().unwrap().get(path).cloned()
}

pub fn forget(path: &Path) {
    registry().lock().unwrap().remove(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_state() {
        let g = Growth::new();
        assert!(!g.is_over());
        g.set_written(10);
        assert_eq!(g.written(), 10);
        g.finish(20);
        assert!(g.is_done() && g.is_over() && !g.is_failed());
        assert_eq!(g.written(), 20);
    }

    #[test]
    fn registry_round_trip() {
        let p = Path::new("growth-test-file.m4a");
        assert!(lookup(p).is_none());
        let g = Growth::new();
        register(p, g.clone());
        assert!(Arc::ptr_eq(&lookup(p).unwrap(), &g));
        forget(p);
        assert!(lookup(p).is_none());
    }
}
