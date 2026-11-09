//! The small copy of a song saved on a metered link, and how it is replaced later.
//!
//! A song downloaded with the data-saving mode is the low bitrate AAC that YouTube also
//! offers. So a later Wi-Fi download can replace it, the file is marked by a sidecar with
//! this name next to it. The sidecar has no audio extension, so the library scan ignores it,
//! and it travels with the song when the file is moved or copied.

use std::path::{Path, PathBuf};

/// The sidecar that marks a low bitrate copy of the song at `path`.
pub fn marker(path: &Path) -> PathBuf {
    path.with_extension("meowtify-low")
}

/// Marks `path` as the low bitrate copy. Does nothing when the sidecar cannot be written:
/// the song still plays, it is only not replaced later.
pub fn mark(path: &Path) {
    let _ = std::fs::write(marker(path), b"");
}

/// Forgets the mark, after the high quality copy replaced the low one.
pub fn unmark(path: &Path) {
    let _ = std::fs::remove_file(marker(path));
}

/// Whether `path` is the low bitrate copy.
pub fn marked(path: &Path) -> bool {
    marker(path).is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mark_sits_next_to_the_song_and_is_not_audio() {
        let dir = std::env::temp_dir().join(format!("meowtify-quality-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let song = dir.join("( T_T ) [P2oz2RBtjWQ].m4a");
        std::fs::write(&song, b"audio").unwrap();
        assert!(!marked(&song));
        mark(&song);
        assert!(marked(&song));
        assert_eq!(marker(&song).file_name().and_then(|n| n.to_str()), Some("( T_T ) [P2oz2RBtjWQ].meowtify-low"));
        unmark(&song);
        assert!(!marked(&song));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
