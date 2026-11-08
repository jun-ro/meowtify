//! Cover images. Each one is fetched once, kept on disk, cut to a square and
//! handed to the UI thread. At most `PARALLEL` fetches run at a time.
//!
//! This module also owns the decoded covers in memory, for every page that shows one: rows,
//! results, the queue, playlist collages and the player. Pages ask [`Thumbs::cached`] and keep
//! no copies of their own, so the memory a cover costs is bounded here, in one place.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use meowtify_cache::{decode_square, DiskCache};
use meowtify_provider::fetch_thumbnail;
use slint::{Image, Rgba8Pixel, SharedPixelBuffer};
use tokio::sync::Semaphore;

/// Decoded side in pixels. Rows show 44 px, so this stays sharp at 2x.
pub(crate) const SIDE: u32 = 88;
const PARALLEL: usize = 6;
const DISK_BYTES: u64 = 48 * 1024 * 1024;
/// Decoded covers kept in memory. At 88 x 88 pixels a cover is 31 KB, so this is about 6 MB.
const KEEP_IMAGES: usize = 192;
/// Covers a page may protect from eviction. Playlist collages use four each, so this covers 32
/// playlists. It is below `KEEP_IMAGES`, so protection cannot push memory past the budget.
const MAX_PINNED: usize = 128;

/// Decoded covers, least recently used first out, except the pinned ones.
#[derive(Default)]
struct Images {
    map: HashMap<String, (Image, u64)>,
    tick: u64,
    pinned: HashSet<String>,
}

impl Images {
    fn get(&mut self, id: &str) -> Option<Image> {
        self.tick += 1;
        let tick = self.tick;
        self.map.get_mut(id).map(|e| {
            e.1 = tick;
            e.0.clone()
        })
    }

    fn put(&mut self, id: String, img: Image) {
        self.tick += 1;
        self.map.insert(id, (img, self.tick));
        self.trim();
    }

    /// Replaces the set of covers that stay while pages show them.
    fn pin(&mut self, ids: impl IntoIterator<Item = String>) {
        self.pinned = ids.into_iter().take(MAX_PINNED).collect();
        self.trim();
    }

    fn trim(&mut self) {
        while self.map.len() > KEEP_IMAGES {
            let oldest = self.map.iter().filter(|(k, _)| !self.pinned.contains(*k)).min_by_key(|(_, v)| v.1).map(|(k, _)| k.clone());
            match oldest {
                Some(k) => self.map.remove(&k),
                None => break,
            };
        }
    }
}

/// Called on the UI thread with the id and the image, or `None` when it failed.
type Listener = Box<dyn Fn(&str, Option<&Image>)>;

pub(crate) struct Thumbs {
    rt: tokio::runtime::Handle,
    disk: Option<Arc<DiskCache>>,
    gate: Arc<Semaphore>,
    listeners: RefCell<Vec<Listener>>,
    /// Ids being loaded. Rows, playlists, results and the player ask for the same cover; one
    /// load answers all of them, so a task (with its url and buffers) exists once per id.
    pending: RefCell<HashSet<String>>,
    images: RefCell<Images>,
}

impl Thumbs {
    pub(crate) fn new(rt: tokio::runtime::Handle) -> Thumbs {
        let dir = meowtify_library::config_dir().join("cache").join("thumbs");
        Thumbs {
            rt,
            disk: DiskCache::open(dir, DISK_BYTES).ok().map(Arc::new),
            gate: Arc::new(Semaphore::new(PARALLEL)),
            listeners: RefCell::new(Vec::new()),
            pending: RefCell::new(HashSet::new()),
            images: RefCell::new(Images::default()),
        }
    }

    /// The cover files on disk, for the settings page to size and clear.
    pub(crate) fn disk(&self) -> Option<Arc<DiskCache>> {
        self.disk.clone()
    }

    /// The decoded cover for `id`, when it is in memory.
    pub(crate) fn cached(&self, id: &str) -> Option<Image> {
        self.images.borrow_mut().get(id)
    }

    /// Keeps these covers in memory until the next call. Pages that draw several covers at once,
    /// such as the playlist collages, name them here so they are not pushed out by scrolling.
    pub(crate) fn pin(&self, ids: impl IntoIterator<Item = String>) {
        self.images.borrow_mut().pin(ids);
    }

    pub(crate) fn listen(&self, f: impl Fn(&str, Option<&Image>) + 'static) {
        self.listeners.borrow_mut().push(Box::new(f));
    }

    /// Starts loading the image for `id` from `url`. The answer goes to the listeners. A cover
    /// that is in memory is not loaded again: ask [`cached`](Self::cached) first.
    pub(crate) fn fetch(&self, id: &str, url: &str) {
        if self.images.borrow().map.contains_key(id) {
            return;
        }
        if !self.pending.borrow_mut().insert(id.to_string()) {
            return; // already on its way
        }
        let (id, url) = (id.to_string(), url.to_string());
        let (disk, gate) = (self.disk.clone(), self.gate.clone());
        self.rt.spawn(async move {
            let _permit = gate.acquire_owned().await.ok();
            let key = id.clone();
            let cached = match &disk {
                Some(d) => {
                    let (d, k) = (d.clone(), key.clone());
                    tokio::task::spawn_blocking(move || d.get(&k)).await.ok().flatten()
                }
                None => None,
            };
            let bytes = match cached {
                Some(b) => Some(Arc::new(b)),
                None => match fetch_thumbnail(&url).await {
                    Ok(b) => {
                        let b = Arc::new(b);
                        if let Some(d) = &disk {
                            // Written while the image decodes, without a second copy of the
                            // bytes. A failed write only means a fetch next time.
                            let (d, k, b2) = (d.clone(), key.clone(), b.clone());
                            let _ = tokio::task::spawn_blocking(move || d.put(&k, &b2));
                        }
                        Some(b)
                    }
                    Err(_) => None,
                },
            };
            let buf = match bytes {
                Some(b) => tokio::task::spawn_blocking(move || {
                    let d = decode_square(&b, SIDE).ok()?;
                    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(d.width, d.height);
                    buf.make_mut_bytes().copy_from_slice(&d.rgba);
                    Some(buf)
                })
                .await
                .ok()
                .flatten(),
                None => None,
            };
            let _ = slint::invoke_from_event_loop(move || {
                crate::THUMBS.with(|t| {
                    if let Some(t) = t.borrow().as_ref() {
                        t.pending.borrow_mut().remove(&id);
                        let img = buf.map(Image::from_rgba8);
                        if let Some(img) = &img {
                            t.images.borrow_mut().put(id.clone(), img.clone());
                        }
                        for l in t.listeners.borrow().iter() {
                            l(&id, img.as_ref());
                        }
                    }
                });
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_use_is_bounded_whatever_is_scrolled_past() {
        let mut images = Images::default();
        for i in 0..2000 {
            images.put(format!("v{i}"), Image::default());
            assert!(images.map.len() <= KEEP_IMAGES);
        }
        assert_eq!(images.map.len(), KEEP_IMAGES);
        assert!(images.get("v1999").is_some() && images.get("v0").is_none(), "the oldest went first");
    }

    #[test]
    fn using_a_cover_keeps_it() {
        let mut images = Images::default();
        for i in 0..KEEP_IMAGES {
            images.put(format!("v{i}"), Image::default());
        }
        assert!(images.get("v0").is_some()); // v0 is now the newest
        images.put("new".into(), Image::default());
        assert!(images.get("v0").is_some(), "the one just used stayed");
        assert!(images.get("v1").is_none(), "the one least used went");
    }

    #[test]
    fn pinned_covers_stay_but_cannot_exceed_the_budget() {
        let mut images = Images::default();
        images.pin((0..400).map(|i| format!("p{i}")));
        assert_eq!(images.pinned.len(), MAX_PINNED, "protection has a ceiling");
        for i in 0..2000 {
            images.put(format!("p{}", i % 400), Image::default());
            images.put(format!("other{i}"), Image::default());
            assert!(images.map.len() <= KEEP_IMAGES, "{}", images.map.len());
        }
        assert!((0..MAX_PINNED).all(|i| images.map.contains_key(&format!("p{i}"))), "pinned covers were never evicted");
    }
}
