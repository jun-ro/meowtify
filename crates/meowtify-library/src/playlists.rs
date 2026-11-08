//! Local playlists: named lists of track files, stored in `playlists.json` next
//! to the settings. A playlist holds file paths, so the music folder stays the
//! source of truth for tags and covers.

use std::path::PathBuf;
use std::sync::{Mutex, RwLock};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaylistData {
    pub id: String,
    pub name: String,
    /// Track file paths in play order.
    pub tracks: Vec<String>,
    /// A cover image the user chose (a file path). `None` means the automatic cover.
    pub cover: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct FileData {
    playlists: Vec<PlaylistData>,
}

/// All playlists. Changes are written to disk at once when a file is set.
#[derive(Default)]
pub struct Playlists {
    file: Option<PathBuf>,
    lists: RwLock<Vec<PlaylistData>>,
    /// The last write failure, cleared by the next write that works. `None` when saved.
    save_error: Mutex<Option<String>>,
}

impl Playlists {
    /// Playlists that live in memory only.
    pub fn in_memory() -> Playlists {
        Playlists::default()
    }

    /// Reads `file`. A missing file gives no playlists. A damaged file is moved to
    /// `<name>.json.bad` so the next save cannot overwrite it, and gives no playlists.
    pub fn load(file: PathBuf) -> Playlists {
        let mut error = None;
        let data: FileData = match std::fs::read(&file) {
            // Editors on Windows may add a byte order mark; JSON parsers reject it.
            Ok(b) => match serde_json::from_slice(b.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&b)) {
                Ok(d) => d,
                Err(e) => {
                    let bad = file.with_extension("json.bad");
                    error = Some(match std::fs::rename(&file, &bad) {
                        Ok(()) => format!("Playlists file is damaged ({e}); kept as {}", bad.display()),
                        Err(r) => format!("Playlists file is damaged ({e}) and could not be set aside: {r}"),
                    });
                    FileData::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => FileData::default(),
            Err(e) => {
                error = Some(format!("Could not read playlists: {e}"));
                FileData::default()
            }
        };
        Playlists { file: Some(file), lists: RwLock::new(data.playlists), save_error: Mutex::new(error) }
    }

    /// Why the playlists are not safely on disk, or `None` when they are.
    pub fn save_error(&self) -> Option<String> {
        self.save_error.lock().unwrap().clone()
    }

    fn save(&self, lists: &[PlaylistData]) {
        let Some(file) = &self.file else { return };
        let result = (|| -> Result<(), String> {
            let bytes = serde_json::to_vec_pretty(&FileData { playlists: lists.to_vec() }).map_err(|e| e.to_string())?;
            if let Some(dir) = file.parent() {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
            let tmp = file.with_extension("json.tmp");
            std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
            std::fs::rename(&tmp, file).map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                e.to_string()
            })
        })();
        *self.save_error.lock().unwrap() = result.err().map(|e| format!("Could not save playlists: {e}"));
    }

    /// Reads one playlist in place, without copying its track list.
    pub fn read<R>(&self, id: &str, f: impl FnOnce(&PlaylistData) -> R) -> Option<R> {
        self.lists.read().unwrap().iter().find(|p| p.id == id).map(f)
    }

    /// The id and name of the playlists from `offset` on, at most `limit`, with how many there
    /// are in all. Their tracks are not copied.
    pub fn names(&self, offset: usize, limit: usize) -> (Vec<(String, String)>, usize) {
        let lists = self.lists.read().unwrap();
        let page = lists.iter().skip(offset).take(limit).map(|p| (p.id.clone(), p.name.clone())).collect();
        (page, lists.len())
    }

    /// Every playlist with its tracks.
    pub fn all(&self) -> Vec<PlaylistData> {
        self.lists.read().unwrap().clone()
    }

    pub fn get(&self, id: &str) -> Option<PlaylistData> {
        self.lists.read().unwrap().iter().find(|p| p.id == id).cloned()
    }

    /// Makes a playlist and returns its id. An empty name becomes "New playlist".
    pub fn create(&self, name: &str) -> String {
        let name = clean_name(name);
        let mut lists = self.lists.write().unwrap();
        let mut n = lists.iter().filter_map(|p| p.id.strip_prefix("pl").and_then(|s| s.parse::<u64>().ok())).max().unwrap_or(0);
        n += 1;
        let id = format!("pl{n}");
        lists.push(PlaylistData { id: id.clone(), name, tracks: Vec::new(), cover: None });
        self.save(&lists);
        id
    }

    pub fn rename(&self, id: &str, name: &str) {
        let mut lists = self.lists.write().unwrap();
        if let Some(p) = lists.iter_mut().find(|p| p.id == id) {
            p.name = clean_name(name);
            self.save(&lists);
        }
    }

    /// Removes a playlist. Returns its cover file, which the caller may delete.
    pub fn delete(&self, id: &str) -> Option<String> {
        let mut lists = self.lists.write().unwrap();
        let cover = lists.iter().find(|p| p.id == id).and_then(|p| p.cover.clone());
        let before = lists.len();
        lists.retain(|p| p.id != id);
        if lists.len() != before {
            self.save(&lists);
        }
        cover
    }

    /// Sets or clears the chosen cover. Returns the previous cover file, which the
    /// caller may delete.
    pub fn set_cover(&self, id: &str, cover: Option<String>) -> Option<String> {
        let mut lists = self.lists.write().unwrap();
        let p = lists.iter_mut().find(|p| p.id == id)?;
        let old = std::mem::replace(&mut p.cover, cover);
        self.save(&lists);
        old
    }

    /// Appends a track. Returns `false` when the playlist is missing or already has it.
    pub fn add(&self, id: &str, uri: &str) -> bool {
        let mut lists = self.lists.write().unwrap();
        let Some(p) = lists.iter_mut().find(|p| p.id == id) else { return false };
        if p.tracks.iter().any(|t| t == uri) {
            return false;
        }
        p.tracks.push(uri.to_string());
        self.save(&lists);
        true
    }

    /// Takes a track out of every playlist (its file is gone).
    pub fn remove_everywhere(&self, uri: &str) {
        let mut lists = self.lists.write().unwrap();
        let mut changed = false;
        for p in lists.iter_mut() {
            let before = p.tracks.len();
            p.tracks.retain(|t| t != uri);
            changed |= p.tracks.len() != before;
        }
        if changed {
            self.save(&lists);
        }
    }

    pub fn remove(&self, id: &str, uri: &str) {
        let mut lists = self.lists.write().unwrap();
        if let Some(p) = lists.iter_mut().find(|p| p.id == id) {
            let before = p.tracks.len();
            p.tracks.retain(|t| t != uri);
            if p.tracks.len() != before {
                self.save(&lists);
            }
        }
    }
}

fn clean_name(name: &str) -> String {
    let n: String = name.trim().chars().filter(|c| !c.is_control()).take(80).collect();
    if n.trim().is_empty() {
        "New playlist".to_string()
    } else {
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_add_remove_rename_delete() {
        let p = Playlists::in_memory();
        let a = p.create("  Road trip ");
        let b = p.create("");
        assert_ne!(a, b);
        assert_eq!(p.get(&a).unwrap().name, "Road trip");
        assert_eq!(p.get(&b).unwrap().name, "New playlist");
        assert!(p.add(&a, "x.m4a"));
        assert!(!p.add(&a, "x.m4a"), "no duplicates");
        assert!(!p.add("missing", "x.m4a"));
        assert!(p.add(&a, "y.m4a"));
        p.remove(&a, "x.m4a");
        assert_eq!(p.get(&a).unwrap().tracks, ["y.m4a"]);
        p.add(&b, "y.m4a");
        p.remove_everywhere("y.m4a");
        assert!(p.get(&a).unwrap().tracks.is_empty() && p.get(&b).unwrap().tracks.is_empty());
        p.add(&a, "y.m4a");
        p.rename(&a, "Drive");
        assert_eq!(p.get(&a).unwrap().name, "Drive");
        assert_eq!(p.set_cover(&a, Some("c1.png".into())), None);
        assert_eq!(p.set_cover(&a, Some("c2.png".into())).as_deref(), Some("c1.png"));
        assert_eq!(p.get(&a).unwrap().cover.as_deref(), Some("c2.png"));
        assert_eq!(p.delete(&a).as_deref(), Some("c2.png"));
        assert!(p.get(&a).is_none());
        // Ids are not reused while a higher one exists.
        assert_eq!(p.create("again"), "pl3");
    }

    #[test]
    fn saves_and_reloads() {
        let dir = std::env::temp_dir().join(format!("meowtify-pl-{}", std::process::id()));
        let file = dir.join("playlists.json");
        let _ = std::fs::remove_dir_all(&dir);
        {
            let p = Playlists::load(file.clone());
            let id = p.create("Keep");
            p.add(&id, "a.m4a");
        }
        let again = Playlists::load(file);
        let all = again.all();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].tracks, ["a.m4a"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
