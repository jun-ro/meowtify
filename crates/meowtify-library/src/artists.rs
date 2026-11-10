//! A small sidecar mapping a download's id to its artist.
//!
//! A file's artist should not depend on its tags alone: a download whose tag write failed, or
//! a desktop yt-dlp download without ffmpeg, carries no artist tag, and there is no way to get
//! it back offline. When a download finishes the app knows the artist from the search result,
//! so it is remembered here and used by the scan when the tag names no artist.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Most artists kept. The oldest entry is dropped first.
pub const MAX_ARTISTS: usize = 20_000;

/// A failed repair is not tried again for this long.
const RETRY_AFTER: u64 = 24 * 60 * 60;

/// The text on disk. Artists are a list, oldest first, because JSON objects do not keep
/// insertion order, and the oldest entries are the ones dropped when the cap is reached.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct FileData {
    artists: Vec<(String, String)>,
    /// Download id and the time (Unix seconds) its repair last failed.
    failed: Vec<(String, u64)>,
}

#[derive(Default)]
struct State {
    loaded: bool,
    /// Insertion order, oldest first. `artists` holds the values.
    order: VecDeque<String>,
    artists: HashMap<String, String>,
    failed: HashMap<String, u64>,
}

impl State {
    fn load(&mut self, data: FileData, cap: usize) {
        for (id, artist) in data.artists {
            if self.artists.insert(id.clone(), artist).is_none() {
                self.order.push_back(id);
            }
        }
        while self.order.len() > cap {
            if let Some(id) = self.order.pop_front() {
                self.artists.remove(&id);
            }
        }
        self.failed.extend(data.failed);
    }

    /// Adds or updates one entry. Returns whether anything changed.
    fn insert(&mut self, id: &str, artist: String, cap: usize) -> bool {
        if let Some(old) = self.artists.get_mut(id) {
            if *old == artist {
                return false;
            }
            *old = artist;
            return true;
        }
        self.order.push_back(id.to_string());
        self.artists.insert(id.to_string(), artist);
        while self.order.len() > cap {
            if let Some(old) = self.order.pop_front() {
                self.artists.remove(&old);
            }
        }
        true
    }
}

/// The artists known for downloads, saved as JSON in `artists.json`.
pub struct ArtistIndex {
    path: PathBuf,
    cap: usize,
    /// Serializes writers, so two saves cannot interleave their temporary files.
    writing: Mutex<()>,
    state: Mutex<State>,
}

impl ArtistIndex {
    pub fn new(path: PathBuf) -> ArtistIndex {
        ArtistIndex::with_cap(path, MAX_ARTISTS)
    }

    fn with_cap(path: PathBuf, cap: usize) -> ArtistIndex {
        ArtistIndex { path, cap, writing: Mutex::new(()), state: Mutex::new(State::default()) }
    }

    /// The index the app uses: `config_dir()/artists.json`, read on first use.
    pub fn local() -> &'static ArtistIndex {
        static LOCAL: OnceLock<ArtistIndex> = OnceLock::new();
        LOCAL.get_or_init(|| ArtistIndex::new(crate::settings::config_dir().join("artists.json")))
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // A panic while holding the lock must not take the index down with it.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Reads the file once. The lock is not held across the read.
    fn ensure_loaded(&self) {
        if self.state().loaded {
            return;
        }
        let data = std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let mut state = self.state();
        if state.loaded {
            return; // another thread loaded it while this one read the file
        }
        state.load(data, self.cap);
        state.loaded = true;
    }

    /// The artist remembered for a download id.
    pub fn get(&self, id: &str) -> Option<String> {
        self.ensure_loaded();
        self.state().artists.get(id.trim()).cloned()
    }

    /// Remembers the artist of a download. An empty or unknown artist is ignored.
    /// Returns whether the index changed.
    pub fn set(&self, id: &str, artist: &str) -> bool {
        let (id, Some(artist)) = (id.trim(), clean_artist(artist)) else { return false };
        if id.is_empty() {
            return false;
        }
        self.ensure_loaded();
        let changed = self.state().insert(id, artist, self.cap);
        if changed {
            let _ = self.save();
        }
        changed
    }

    /// Notes that getting this download's artist failed, so it is not tried again today.
    pub fn note_failed(&self, id: &str) {
        let id = id.trim();
        if id.is_empty() {
            return;
        }
        self.ensure_loaded();
        let now = unix_now();
        let changed = {
            let mut state = self.state();
            let old = state.failed.insert(id.to_string(), now);
            old != Some(now)
        };
        if changed {
            let _ = self.save();
        }
    }

    /// Whether this id's repair failed in the last 24 hours.
    pub fn failed_recently(&self, id: &str) -> bool {
        self.ensure_loaded();
        let at = self.state().failed.get(id.trim()).copied();
        at.is_some_and(|at| unix_now().saturating_sub(at) < RETRY_AFTER)
    }

    /// Forgets a failure, after the artist was found anyway.
    pub fn clear_failed(&self, id: &str) {
        self.ensure_loaded();
        let removed = self.state().failed.remove(id.trim()).is_some();
        if removed {
            let _ = self.save();
        }
    }

    /// Writes the file through a temporary file, so a crash cannot leave half of it.
    /// The state lock is only held while the contents are copied out.
    fn save(&self) -> std::io::Result<()> {
        self.ensure_loaded();
        let _one_writer = self.writing.lock().unwrap_or_else(|e| e.into_inner());
        let data = {
            let state = self.state();
            FileData {
                artists: state.order.iter().filter_map(|id| state.artists.get(id).map(|a| (id.clone(), a.clone()))).collect(),
                failed: state.failed.iter().map(|(id, at)| (id.clone(), *at)).collect(),
            }
        };
        let bytes = serde_json::to_vec(&data).map_err(std::io::Error::other)?;
        if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = temp_path(&self.path);
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &self.path)
    }
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// An artist tag as it should be shown: trimmed, without YouTube's ` - Topic` suffix, or
/// `None` when nothing is left. `None` means no artist is known, not that it is unknown.
pub fn clean_artist(artist: &str) -> Option<String> {
    /// YouTube's suffix on an artist channel's auto-generated uploads.
    const TOPIC: &str = " - Topic";
    let artist = artist.trim();
    let mut name = artist;
    if artist.len() >= TOPIC.len() && artist[artist.len() - TOPIC.len()..].eq_ignore_ascii_case(TOPIC) {
        name = artist[..artist.len() - TOPIC.len()].trim_end_matches([' ', '-']);
    }
    let name = name.trim();
    if name.is_empty()
        || name.chars().all(|c| c == '-')
        || name.trim_start_matches([' ', '-']).eq_ignore_ascii_case("topic")
        || name.eq_ignore_ascii_case("unknown artist")
    {
        return None;
    }
    Some(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_index(name: &str) -> (ArtistIndex, PathBuf) {
        let dir = std::env::temp_dir().join(format!("meowtify-artists-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (ArtistIndex::new(dir.join("artists.json")), dir)
    }

    #[test]
    fn an_artist_survives_a_restart() {
        let (index, dir) = temp_index("roundtrip");
        assert!(index.set("dQw4w9WgXcQ", "Rick Astley"));
        assert!(!index.set("dQw4w9WgXcQ", "Rick Astley"), "the same value is not written again");
        let reopened = ArtistIndex::new(dir.join("artists.json"));
        assert_eq!(reopened.get("dQw4w9WgXcQ").as_deref(), Some("Rick Astley"));
        assert_eq!(reopened.get("nope"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_oldest_artist_is_dropped_at_the_cap() {
        let (index, dir) = temp_index("cap");
        let index = ArtistIndex { cap: 3, ..index };
        for (i, id) in ["aaaaaaaaaaa", "bbbbbbbbbbb", "ccccccccccc", "ddddddddddd"].into_iter().enumerate() {
            assert!(index.set(id, &format!("Artist {i}")));
        }
        assert_eq!(index.get("aaaaaaaaaaa"), None, "the oldest one went");
        for id in ["bbbbbbbbbbb", "ccccccccccc", "ddddddddddd"] {
            assert!(index.get(id).is_some());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_or_unknown_artist_is_not_remembered() {
        let (index, dir) = temp_index("empty");
        assert!(!index.set("dQw4w9WgXcQ", "  "));
        assert!(!index.set("dQw4w9WgXcQ", "Unknown artist"));
        assert!(!index.set("", "Somebody"));
        assert_eq!(index.get("dQw4w9WgXcQ"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failure_is_remembered_for_a_day() {
        let (index, dir) = temp_index("failed");
        assert!(!index.failed_recently("dQw4w9WgXcQ"));
        index.note_failed("dQw4w9WgXcQ");
        assert!(index.failed_recently("dQw4w9WgXcQ"), "not retried this launch");
        let reopened = ArtistIndex::new(dir.join("artists.json"));
        assert!(reopened.failed_recently("dQw4w9WgXcQ"), "and not after a restart either");
        reopened.clear_failed("dQw4w9WgXcQ");
        assert!(!reopened.failed_recently("dQw4w9WgXcQ"));
        // An old failure is tried again.
        reopened.note_failed("aaaaaaaaaaa");
        {
            let mut state = reopened.state();
            state.failed.insert("aaaaaaaaaaa".into(), unix_now().saturating_sub(RETRY_AFTER + 1));
        }
        assert!(!reopened.failed_recently("aaaaaaaaaaa"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_file_reads_as_empty() {
        let (index, dir) = temp_index("damaged");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("artists.json"), b"not json").unwrap();
        assert_eq!(index.get("dQw4w9WgXcQ"), None);
        assert!(index.set("dQw4w9WgXcQ", "A"), "and it can be written again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_topic_suffix_is_dropped() {
        assert_eq!(clean_artist("Rick Astley - Topic").as_deref(), Some("Rick Astley"));
        assert_eq!(clean_artist("  Rick Astley - topic  ").as_deref(), Some("Rick Astley"));
        assert_eq!(clean_artist("A - TopicB").as_deref(), Some("A - TopicB"));
        assert_eq!(clean_artist(" - Topic"), None);
        assert_eq!(clean_artist("   "), None);
        assert_eq!(clean_artist("Unknown Artist"), None);
    }
}
