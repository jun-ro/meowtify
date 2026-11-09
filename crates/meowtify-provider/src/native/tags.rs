//! Title and artist tags, written in process with lofty.
//!
//! The desktop path only adds tags when ffmpeg is there, and this keeps the same rule:
//! the caller decides whether to tag. Nothing is re-encoded; lofty edits the existing
//! MP4 or MP3 metadata.

use std::path::Path;

use lofty::config::WriteOptions;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::prelude::Accessor;
use lofty::tag::Tag;
use lofty::read_from_path;

use crate::{Item, ProviderError, Result};

/// Writes `item`'s title and artist into the file at `path`.
pub(crate) fn tag(path: &Path, item: &Item) -> Result<()> {
    let mut file = read_from_path(path).map_err(|e| ProviderError::Parse(format!("the downloaded file has no readable tags: {e}")))?;
    if file.primary_tag().is_none() {
        file.insert_tag(Tag::new(file.primary_tag_type()));
    }
    let tag = file
        .primary_tag_mut()
        .ok_or_else(|| ProviderError::Parse("the downloaded file cannot hold tags".into()))?;
    tag.set_title(item.title.clone());
    if !item.artist.trim().is_empty() {
        tag.set_artist(item.artist.clone());
    }
    file.save_to_path(path, WriteOptions::default())
        .map_err(|e| ProviderError::Io(format!("could not save tags: {e}")))
}

/// Tags a copy and moves it over the song, so a file the player still has open is never
/// rewritten under it. Where the move is refused (Windows, a song that is playing), the
/// copy is removed and the song stays untagged; the next scan can tag it again.
pub(crate) async fn tag_copy(path: &Path, item: &Item) -> Result<()> {
    let tmp = path.with_extension("tagging");
    tokio::fs::copy(path, &tmp).await.map_err(|e| ProviderError::Io(e.to_string()))?;
    let (source, item) = (tmp.clone(), item.clone());
    let tagged = tokio::task::spawn_blocking(move || tag(&source, &item))
        .await
        .map_err(|e| ProviderError::Io(e.to_string()))?;
    if let Err(e) = tagged {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(ProviderError::Io(format!("could not put the tags in place: {e}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_that_is_not_audio_is_refused() {
        let dir = std::env::temp_dir().join(format!("meowtify-tags-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("not-a-song.m4a");
        std::fs::write(&path, b"this is not an mp4").unwrap();
        let item = Item {
            provider: "youtube",
            id: "abc".into(),
            title: "T".into(),
            artist: "A".into(),
            duration_secs: None,
            url: String::new(),
            thumbnail: None,
        };
        assert!(tag(&path, &item).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The copy the tags are written on never leaks into the music folder.
    #[tokio::test]
    async fn a_failed_tag_copy_leaves_no_file_behind() {
        let dir = std::env::temp_dir().join(format!("meowtify-tags-copy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("not-a-song.m4a");
        std::fs::write(&path, b"this is not an mp4").unwrap();
        let item = Item {
            provider: "youtube",
            id: "abc".into(),
            title: "T".into(),
            artist: "A".into(),
            duration_secs: None,
            url: String::new(),
            thumbnail: None,
        };
        assert!(tag_copy(&path, &item).await.is_err());
        assert!(!path.with_extension("tagging").exists());
        assert_eq!(std::fs::read(&path).unwrap(), b"this is not an mp4", "the song is untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Needs a real m4a: `MEOWTIFY_TEST_M4A=path cargo test -p meowtify-provider --features native-extract retags -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn retags_a_real_file() {
        let src = std::env::var("MEOWTIFY_TEST_M4A").expect("set MEOWTIFY_TEST_M4A");
        let dir = std::env::temp_dir().join(format!("meowtify-tags-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("song [abc].m4a");
        std::fs::copy(&src, &path).unwrap();
        let item = Item {
            provider: "youtube",
            id: "abc".into(),
            title: "Tagged Title".into(),
            artist: "Some Artist".into(),
            duration_secs: None,
            url: String::new(),
            thumbnail: None,
        };
        tag(&path, &item).unwrap();
        let file = lofty::read_from_path(&path).unwrap();
        let tag = file.primary_tag().expect("a tag was written");
        assert_eq!(tag.title().as_deref(), Some("Tagged Title"));
        assert_eq!(tag.artist().as_deref(), Some("Some Artist"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
