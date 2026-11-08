//! The local music library. The files in the user's folder are the source of
//! truth. This crate scans them into [`Track`] values and serves them through
//! [`MetadataSource`].

mod playlists;
mod scan;
mod settings;

use std::path::PathBuf;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use meowtify_core::{Error, MetadataSource, Page, Playlist, Result, Track};

pub use playlists::{PlaylistData, Playlists};
pub use scan::{find_audio_files, is_audio, scan, AUDIO_EXTENSIONS};
pub use settings::{config_dir, Settings, AHEAD_RANGE, BEHIND_RANGE};

/// Most transient tracks kept at once. The oldest one is dropped first.
const MAX_EXTRAS: usize = 256;

/// Transient tracks with insertion order, so eviction removes one entry at a time.
#[derive(Default)]
struct Extras {
    map: HashMap<String, Track>,
    order: VecDeque<String>,
}

/// Lookups by file path and by the id in a download's name, rebuilt whenever the list changes.
/// The UI asks for these on every progress tick and for every visible row; a scan of the
/// whole library each time does not scale.
#[derive(Default)]
struct Lookup {
    by_uri: HashMap<Arc<str>, Track>,
    by_id: HashMap<String, Track>,
}

impl Lookup {
    /// The full index of a list, built after a scan. For one file use `insert` or `remove`.
    fn of(list: &[Track]) -> Lookup {
        let mut l = Lookup { by_uri: HashMap::with_capacity(list.len()), by_id: HashMap::new() };
        for t in list {
            l.insert(t);
        }
        l
    }

    /// Adds one track. When two files carry the same download id, the first one added keeps it.
    fn insert(&mut self, t: &Track) {
        if let Some(id) = meowtify_core::download_id(&t.uri) {
            self.by_id.entry(id.to_string()).or_insert_with(|| t.clone());
        }
        self.by_uri.insert(t.uri.clone(), t.clone());
    }

    /// Takes one track out. `rest` is the list without it: another file with the same download
    /// id takes over the id.
    fn remove(&mut self, uri: &str, rest: &[Track]) {
        self.by_uri.remove(uri);
        let Some(id) = meowtify_core::download_id(uri) else { return };
        if self.by_id.get(id).is_some_and(|t| t.uri.as_ref() == uri) {
            self.by_id.remove(id);
            if let Some(t) = rest.iter().find(|t| meowtify_core::download_id(&t.uri) == Some(id)) {
                self.by_id.insert(id.to_string(), t.clone());
            }
        }
    }
}

/// A change made to the track list while a scan runs. The scan commit replays it.
enum Change {
    Add(Track),
    Remove(String),
}

#[derive(Default)]
pub struct Library {
    dir: RwLock<Option<PathBuf>>,
    /// Bumped on every folder change. A scan that started under an older value is dropped.
    dir_gen: AtomicU64,
    /// Held for the whole of a scan, so scans commit in order.
    scan_lock: Mutex<()>,
    /// `Some` while a scan runs. Collects adds and removes made in the meantime.
    journal: Mutex<Option<Vec<Change>>>,
    tracks: RwLock<Arc<[Track]>>,
    /// Always describes `tracks`; written only while the `tracks` write lock is held.
    lookup: RwLock<Lookup>,
    /// Tracks the player can play that are not part of the library (autoplay picks).
    /// They are found by `find`, never listed, and never searched.
    extras: RwLock<Extras>,
    playlists: Playlists,
}

impl Library {
    pub fn new(dir: Option<PathBuf>) -> Library {
        Library { dir: RwLock::new(dir), tracks: RwLock::new(Arc::from(Vec::new())), playlists: Playlists::in_memory(), ..Library::default() }
    }

    /// Keeps playlists in `file` (read now, written on every change).
    pub fn with_playlist_file(mut self, file: PathBuf) -> Library {
        self.playlists = Playlists::load(file);
        self
    }

    pub fn playlists(&self) -> &Playlists {
        &self.playlists
    }

    /// The tracks of a playlist that are in the library, in playlist order.
    /// Files that are gone are skipped.
    pub fn playlist_tracks(&self, id: &str) -> Vec<Track> {
        self.playlist_slice(id, 0, usize::MAX).map(|(items, _)| items).unwrap_or_default()
    }

    /// `limit` tracks of a playlist from `offset` on, with how many it has in all. Only the
    /// tracks asked for are copied; the paths are looked up in the index the library keeps.
    fn playlist_slice(&self, id: &str, offset: usize, limit: usize) -> Option<(Vec<Track>, usize)> {
        let lookup = self.lookup.read().unwrap();
        self.playlists.read(id, |p| {
            let present = p.tracks.iter().filter_map(|u| lookup.by_uri.get(u.as_str()));
            let total = present.clone().count();
            (present.skip(offset).take(limit).cloned().collect(), total)
        })
    }

    pub fn dir(&self) -> Option<PathBuf> {
        self.dir.read().unwrap().clone()
    }

    pub fn set_dir(&self, dir: Option<PathBuf>) {
        *self.dir.write().unwrap() = dir;
        self.dir_gen.fetch_add(1, Ordering::SeqCst);
    }

    /// Scans the folder and replaces the track list. Blocks, so call it from a
    /// blocking thread. Returns the number of tracks listed afterwards.
    ///
    /// Files added or removed while the scan ran are replayed onto the result. A
    /// scan whose folder changed meanwhile is dropped, so it cannot overwrite the
    /// list of the new folder.
    pub fn rescan_blocking(&self) -> usize {
        let _one_scan = self.scan_lock.lock().unwrap();
        let generation = self.dir_gen.load(Ordering::SeqCst);
        let dir = self.dir();
        *self.journal.lock().unwrap() = Some(Vec::new());
        let mut tracks = match dir {
            Some(d) if d.is_dir() => scan(&d),
            _ => Vec::new(),
        };
        let mut guard = self.tracks.write().unwrap();
        let changes = self.journal.lock().unwrap().take().unwrap_or_default();
        if self.dir_gen.load(Ordering::SeqCst) != generation {
            return guard.len();
        }
        for change in changes {
            match change {
                Change::Add(t) => {
                    if !tracks.iter().any(|x| x.uri == t.uri) {
                        let key = scan::sort_key(&t);
                        let at = tracks.partition_point(|x| scan::sort_key(x) <= key);
                        tracks.insert(at, t);
                    }
                }
                Change::Remove(uri) => tracks.retain(|x| x.uri.as_ref() != uri),
            }
        }
        let n = tracks.len();
        *self.lookup.write().unwrap() = Lookup::of(&tracks);
        *guard = tracks.into();
        n
    }

    fn record(&self, change: Change) {
        if let Some(j) = self.journal.lock().unwrap().as_mut() {
            j.push(change);
        }
    }

    /// Adds one new file without a full rescan: reads its tags and inserts it in
    /// library order. Returns the track, also when it was already listed. Blocks
    /// for the time of one tag read.
    pub fn add_file(&self, path: &std::path::Path) -> Option<Track> {
        let uri = path.to_string_lossy();
        if let Some(t) = self.find(&uri) {
            return Some(t);
        }
        let track = scan::read_one(path)?;
        Some(self.insert_sorted(track))
    }

    /// Lists a new track in library order and indexes it. If its path is already listed, that
    /// track is returned and nothing changes.
    fn insert_sorted(&self, track: Track) -> Track {
        let mut guard = self.tracks.write().unwrap();
        if let Some(listed) = self.lookup.read().unwrap().by_uri.get(&track.uri) {
            return listed.clone();
        }
        let key = scan::sort_key(&track);
        let at = guard.partition_point(|t| scan::sort_key(t) <= key);
        let mut list = Vec::with_capacity(guard.len() + 1);
        list.extend_from_slice(&guard[..at]);
        list.push(track.clone());
        list.extend_from_slice(&guard[at..]);
        self.lookup.write().unwrap().insert(&track);
        *guard = list.into();
        self.record(Change::Add(track.clone()));
        track
    }

    /// Adds a file that is still downloading, using the details of the search result
    /// instead of its tags. Returns the track, also when it was already listed.
    pub fn add_streaming(&self, path: &std::path::Path, title: &str, artist: &str, duration_ms: u32) -> Track {
        let uri = path.to_string_lossy();
        if let Some(t) = self.find(&uri) {
            return t;
        }
        self.insert_sorted(scan::track_from_meta(path, title, artist, duration_ms))
    }

    /// Makes a file playable without listing it in the library. Used for songs that
    /// autoplay downloads into its own cache folder. Returns the track.
    pub fn add_transient(&self, path: &std::path::Path, title: &str, artist: &str, duration_ms: u32) -> Track {
        let uri = path.to_string_lossy().into_owned();
        let mut extras = self.extras.write().unwrap();
        if let Some(t) = extras.map.get(&uri) {
            return t.clone();
        }
        // Drop the oldest-inserted one at a time, never the whole set.
        while extras.map.len() >= MAX_EXTRAS {
            let Some(oldest) = extras.order.pop_front() else { break };
            extras.map.remove(&oldest);
        }
        let track = scan::track_from_meta(path, title, artist, duration_ms);
        extras.map.insert(uri.clone(), track.clone());
        extras.order.push_back(uri);
        track
    }

    /// Forgets a transient track after its file was deleted.
    pub fn forget_transient(&self, uri: &str) {
        let mut extras = self.extras.write().unwrap();
        if extras.map.remove(uri).is_some() {
            extras.order.retain(|u| u != uri);
        }
    }

    /// Takes a track out of the list after its file was deleted. Returns whether it was there.
    pub fn remove_file(&self, uri: &str) -> bool {
        let mut guard = self.tracks.write().unwrap();
        if !guard.iter().any(|t| t.uri.as_ref() == uri) {
            return false;
        }
        let list: Vec<Track> = guard.iter().filter(|t| t.uri.as_ref() != uri).cloned().collect();
        self.lookup.write().unwrap().remove(uri, &list);
        *guard = list.into();
        self.record(Change::Remove(uri.to_string()));
        true
    }

    /// Current tracks in library order. Cheap to clone.
    pub fn tracks(&self) -> Arc<[Track]> {
        self.tracks.read().unwrap().clone()
    }

    /// A downloaded track by its provider id. Downloads are named `... [id].ext`.
    pub fn find_by_video_id(&self, id: &str) -> Option<Track> {
        self.lookup.read().unwrap().by_id.get(id).cloned()
    }

    /// The playlist as an extended M3U8 file (UTF-8), or `None` when there is no such playlist.
    /// Songs are listed by the full path of their file, so the file works where the music is.
    pub fn playlist_m3u8(&self, id: &str) -> Option<String> {
        let p = self.playlists.get(id)?;
        let mut out = String::from("#EXTM3U\r\n");
        out.push_str(&format!("#PLAYLIST:{}\r\n", one_line(&p.name)));
        for uri in &p.tracks {
            if let Some(t) = self.find(uri) {
                let artist = t.artists.iter().map(|a| a.name.as_ref()).collect::<Vec<_>>().join(", ");
                let label = if artist.is_empty() { one_line(&t.name) } else { format!("{} - {}", one_line(&artist), one_line(&t.name)) };
                out.push_str(&format!("#EXTINF:{},{label}\r\n", t.duration_ms / 1000));
            }
            out.push_str(&one_line(uri));
            out.push_str("\r\n");
        }
        Some(out)
    }

    /// A library track by its file path, or a transient one added for autoplay.
    pub fn find(&self, uri: &str) -> Option<Track> {
        self.lookup.read().unwrap().by_uri.get(uri).cloned().or_else(|| self.extras.read().unwrap().map.get(uri).cloned())
    }
}

fn page(items: &[Track], offset: u32, limit: u32) -> Page<Track> {
    let start = (offset as usize).min(items.len());
    let end = (start + limit as usize).min(items.len());
    Page { items: items[start..end].to_vec(), offset, limit, total: items.len() as u32, has_next: end < items.len() }
}

#[async_trait]
impl MetadataSource for Library {
    async fn user_playlists(&self, offset: u32, limit: u32) -> Result<Page<Playlist>> {
        let (names, total) = self.playlists.names(offset as usize, limit as usize);
        let items: Vec<Playlist> = names.into_iter().map(|(id, name)| Playlist { id: id.as_str().into(), name: name.as_str().into() }).collect();
        let has_next = (offset as usize).saturating_add(items.len()) < total;
        Ok(Page { items, offset, limit, total: total as u32, has_next })
    }

    async fn playlist_items(&self, id: &str, offset: u32, limit: u32) -> Result<Page<Track>> {
        let (items, total) = self.playlist_slice(id, offset as usize, limit as usize).ok_or(Error::NotFound)?;
        let has_next = (offset as usize).saturating_add(items.len()) < total;
        Ok(Page { items, offset, limit, total: total as u32, has_next })
    }

    async fn saved_tracks(&self, offset: u32, limit: u32) -> Result<Page<Track>> {
        Ok(page(&self.tracks(), offset, limit))
    }
}

/// Text on one line: a line break in a name would break the playlist file format.
fn one_line(s: &str) -> String {
    s.replace(['\r', '\n'], " ")
}

#[cfg(test)]
mod export_tests {
    use super::*;

    #[test]
    fn a_playlist_becomes_an_m3u8_file() {
        let lib = Library::new(None);
        let id = lib.playlists().create("Road\ntrip");
        lib.playlists().add(&id, "C:\\Music\\gone.m4a");
        let text = lib.playlist_m3u8(&id).unwrap();
        let lines: Vec<&str> = text.split("\r\n").collect();
        assert_eq!(lines[0], "#EXTM3U");
        assert_eq!(lines[1], "#PLAYLIST:Roadtrip");
        assert_eq!(lines[2], "C:\\Music\\gone.m4a", "a song missing from the library is still listed by path");
        assert!(lib.playlist_m3u8("nope").is_none());
    }
}

#[cfg(test)]
mod lookup_tests {
    use super::*;

    fn lib_with(names: &[&str]) -> Library {
        let lib = Library::new(None);
        for n in names {
            lib.add_streaming(std::path::Path::new(&format!("/m/{n}")), n, "A", 1000);
        }
        lib
    }

    #[test]
    fn one_added_or_removed_file_updates_the_index_without_losing_the_rest() {
        let lib = lib_with(&["One [aaaaaaaaaaa].m4a", "Two [bbbbbbbbbbb].m4a"]);
        assert!(lib.find_by_video_id("aaaaaaaaaaa").is_some());
        let again = lib.add_streaming(std::path::Path::new("/m/One [aaaaaaaaaaa].m4a"), "One", "A", 1000);
        assert_eq!(lib.tracks().len(), 2, "a path already listed is not listed twice");
        assert_eq!(again.uri.as_ref(), "/m/One [aaaaaaaaaaa].m4a");
        assert!(lib.remove_file("/m/One [aaaaaaaaaaa].m4a"));
        assert!(lib.find_by_video_id("aaaaaaaaaaa").is_none() && lib.find("/m/One [aaaaaaaaaaa].m4a").is_none());
        assert!(lib.find_by_video_id("bbbbbbbbbbb").is_some(), "the other file is still indexed");
        assert_eq!(lib.tracks().len(), 1);
    }

    #[test]
    fn a_second_file_with_the_same_id_takes_over_when_the_first_goes() {
        let lib = lib_with(&["A [cccccccccc1].m4a", "B [cccccccccc1].m4a"]);
        let first = lib.find_by_video_id("cccccccccc1").unwrap().uri;
        assert!(lib.remove_file(&first));
        let left = lib.find_by_video_id("cccccccccc1").expect("the copy still answers");
        assert_ne!(left.uri, first);
    }

    #[test]
    fn a_playlist_page_copies_only_its_own_tracks_and_skips_missing_files() {
        let lib = lib_with(&["a.m4a", "b.m4a", "c.m4a", "d.m4a"]);
        let id = lib.playlists().create("p");
        for n in ["/m/d.m4a", "/m/gone.m4a", "/m/a.m4a", "/m/c.m4a"] {
            lib.playlists().add(&id, n);
        }
        let uris = |v: &[Track]| v.iter().map(|t| t.uri.to_string()).collect::<Vec<_>>();
        let (first, total) = lib.playlist_slice(&id, 0, 2).unwrap();
        assert_eq!(total, 3, "the missing file is not counted");
        assert_eq!(uris(&first), ["/m/d.m4a", "/m/a.m4a"]);
        let (last, _) = lib.playlist_slice(&id, 2, 2).unwrap();
        assert_eq!(uris(&last), ["/m/c.m4a"]);
        assert!(lib.playlist_slice("nope", 0, 5).is_none());
        assert!(lib.playlist_slice(&id, 99, 5).unwrap().0.is_empty());
        assert_eq!(lib.playlist_tracks(&id).len(), 3);
        let (names, count) = lib.playlists().names(0, 10);
        assert_eq!((names.len(), count), (1, 1));
    }
}
