//! Walks a folder and reads tags from the audio files in it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lofty::prelude::*;
use meowtify_core::{Album, Artist, Track};

/// Extensions the player can decode.
pub const AUDIO_EXTENSIONS: [&str; 7] = ["m4a", "mp3", "flac", "ogg", "wav", "aac", "mp4"];

pub fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// All audio files under `dir`, without following symlinked folders.
pub fn find_audio_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for entry in rd.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            let p = entry.path();
            if ft.is_dir() {
                stack.push(p);
            } else if ft.is_file() && is_audio(&p) {
                out.push(p);
            }
        }
    }
    out
}

/// FNV-1a as 16 hex digits. Stable id from a path.
fn path_id(p: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in p.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Shares one `Arc` per artist and per album, so 10k tracks do not hold 10k copies.
#[derive(Default)]
struct Interner {
    artists: HashMap<String, Artist>,
    albums: HashMap<(String, String), Arc<Album>>,
}

impl Interner {
    fn artist(&mut self, name: &str) -> Artist {
        self.artists
            .entry(name.to_string())
            .or_insert_with(|| Artist { id: name.into(), name: name.into() })
            .clone()
    }

    fn album(&mut self, name: &str, artist: &Artist) -> Arc<Album> {
        let key = (name.to_string(), artist.name.to_string());
        if let Some(a) = self.albums.get(&key) {
            return a.clone();
        }
        let a = Arc::new(Album {
            id: format!("{}\u{1f}{}", artist.name, name).into(),
            name: name.into(),
            artists: Arc::from(vec![artist.clone()]),
        });
        self.albums.insert(key, a.clone());
        a
    }
}

/// Reads one file. Missing tags fall back to the file name.
fn read_track(path: &Path, interner: &mut Interner) -> Option<Track> {
    let tagged = lofty::read_from_path(path).ok()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("Unknown");
    let title = tag.and_then(|t| t.title().map(|c| c.into_owned())).filter(|s| !s.trim().is_empty());
    let artist_name = tag.and_then(|t| t.artist().map(|c| c.into_owned())).filter(|s| !s.trim().is_empty());
    let album_name = tag.and_then(|t| t.album().map(|c| c.into_owned())).filter(|s| !s.trim().is_empty());

    let artist = interner.artist(artist_name.as_deref().unwrap_or("Unknown artist"));
    let album = album_name.map(|a| interner.album(&a, &artist));
    let uri = path.to_string_lossy().into_owned();
    let name: Arc<str> = title.as_deref().unwrap_or_else(|| name_from_stem(stem, &uri)).into();
    Some(Track {
        id: path_id(&uri).into(),
        uri: uri.into(),
        name,
        artists: Arc::from(vec![artist]),
        album,
        duration_ms: tagged.properties().duration().as_millis().min(u32::MAX as u128) as u32,
    })
}

/// The title for a file with no title tag: its name, without the ` [id]` a download carries.
fn name_from_stem<'a>(stem: &'a str, path: &str) -> &'a str {
    if meowtify_core::download_id(path).is_none() {
        return stem;
    }
    match stem.rfind(" [") {
        Some(i) if stem.ends_with(']') && i > 0 => &stem[..i],
        _ => stem,
    }
}

/// Scans `dir` and returns its tracks sorted by artist, album, title.
/// Files that cannot be read are skipped.
pub fn scan(dir: &Path) -> Vec<Track> {
    let mut interner = Interner::default();
    let mut tracks: Vec<Track> =
        find_audio_files(dir).iter().filter_map(|p| read_track(p, &mut interner)).collect();
    tracks.sort_by_cached_key(sort_key);
    tracks
}

/// Library order: artist, album, title, ignoring case.
pub(crate) fn sort_key(t: &Track) -> (String, String, String) {
    (
        t.artists.first().map(|a| a.name.to_lowercase()).unwrap_or_default(),
        t.album.as_ref().map(|a| a.name.to_lowercase()).unwrap_or_default(),
        t.name.to_lowercase(),
    )
}

/// A track for a file that is still being downloaded, so its tags cannot be read yet.
/// The details come from the search result.
pub(crate) fn track_from_meta(path: &Path, title: &str, artist: &str, duration_ms: u32) -> Track {
    let mut interner = Interner::default();
    let artist = interner.artist(if artist.trim().is_empty() { "Unknown artist" } else { artist.trim() });
    let uri = path.to_string_lossy().into_owned();
    Track {
        id: path_id(&uri).into(),
        uri: uri.into(),
        name: title.trim().into(),
        artists: Arc::from(vec![artist]),
        album: None,
        duration_ms,
    }
}

/// Reads one file. `None` when it cannot be read as music.
pub(crate) fn read_one(path: &Path) -> Option<Track> {
    read_track(path, &mut Interner::default())
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_download_name_loses_its_id_as_a_title() {
        assert_eq!(super::name_from_stem("( T_T ) [P2oz2RBtjWQ]", "/m/( T_T ) [P2oz2RBtjWQ].m4a"), "( T_T )");
        assert_eq!(super::name_from_stem("Song [live]", "/m/Song [live].m4a"), "Song", "any id counts as a download id");
        assert_eq!(super::name_from_stem("Song", "/m/Song.m4a"), "Song");
        assert_eq!(super::name_from_stem("[x]", "/m/[x].m4a"), "[x]", "nothing is left of the name otherwise");
    }

    use super::*;

    #[test]
    fn recognises_audio_extensions_case_insensitively() {
        assert!(is_audio(Path::new("a/b.M4A")));
        assert!(is_audio(Path::new("c.mp3")));
        assert!(!is_audio(Path::new("c.txt")));
        assert!(!is_audio(Path::new("noext")));
    }

    #[test]
    fn path_ids_are_stable_and_distinct() {
        assert_eq!(path_id("a"), path_id("a"));
        assert_ne!(path_id("a"), path_id("b"));
    }

    #[test]
    fn finds_nested_files_and_skips_unreadable_audio() {
        let root = std::env::temp_dir().join(format!("meowtify-scan-{}", std::process::id()));
        let nested = root.join("x").join("y");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("one.mp3"), b"not really audio").unwrap();
        std::fs::write(root.join("note.txt"), b"hi").unwrap();
        assert_eq!(find_audio_files(&root).len(), 1);
        assert!(scan(&root).is_empty(), "bad audio is skipped, not a panic");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
