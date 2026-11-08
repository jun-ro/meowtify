//! Music providers and the download queue.
//!
//! A [`Provider`] can search, resolve a pasted link, and download one item
//! into a folder. YouTube is the first provider and works by running the
//! `yt-dlp` executable. [`DownloadManager`] runs downloads one at a time and
//! reports progress.

mod audius;
mod lyrics;
mod manager;
mod net;
mod recommend;
mod soundcloud;
mod stream;
mod update;
mod youtube;
mod ytdlp;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

pub use recommend::{
    main_artist, normalize, pick_match, resolve, song_key, variants, valid_lastfm_key, Candidate, DeezerRadio, LastFmSimilar, Recommender, Seed, YtMusicRadio,
};
pub use net::{proxy, set_proxy, validate_proxy};
pub use manager::{DownloadManager, Job, JobState, DEFAULT_PARALLEL};
pub use audius::Audius;
pub use lyrics::{find as find_lyrics, Line as LyricLine, Lyrics, Query as LyricsQuery};
pub use soundcloud::SoundCloud;
pub mod app_update {
    pub use crate::update::{check, clean_up, install, Release};
}
pub use youtube::{validate_youtube_url, yt_dlp_version, Tools, YouTube};
pub use ytdlp::{fetch_thumbnail, suggest, UpdateOutcome, YtDlp};

pub type Result<T> = std::result::Result<T, ProviderError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderError {
    /// A required program was not found. The payload names it.
    ToolMissing(&'static str),
    /// The link or query was refused before running anything.
    Invalid(String),
    /// The program failed. The payload is its last error line.
    Tool(String),
    /// The program's output could not be understood.
    Parse(String),
    Io(String),
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderError::ToolMissing(t) => write!(f, "{t} was not found. Install it or put it in the Meowtify tools folder."),
            ProviderError::Invalid(m) => write!(f, "{m}"),
            ProviderError::Tool(m) => write!(f, "{m}"),
            ProviderError::Parse(m) => write!(f, "unexpected output: {m}"),
            ProviderError::Io(m) => write!(f, "io error: {m}"),
        }
    }
}

impl std::error::Error for ProviderError {}

/// One downloadable thing found by a provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// Provider id, such as `youtube`. Together with `id` it is unique.
    pub provider: &'static str,
    pub id: String,
    pub title: String,
    /// Channel or artist, when known.
    pub artist: String,
    pub duration_secs: Option<u32>,
    /// Page to open or download from.
    pub url: String,
    /// Small cover image, when the provider has one.
    pub thumbnail: Option<String>,
}

/// Called with a percentage from 0.0 to 100.0.
pub type ProgressFn = Arc<dyn Fn(f32) + Send + Sync>;

/// Called with the path of a file that can already be played while it is still downloading.
pub type ReadyFn = Arc<dyn Fn(&Path) + Send + Sync>;

#[async_trait]
pub trait Provider: Send + Sync {
    /// Text search. At most `limit` items.
    async fn search(&self, query: &str, limit: u32) -> Result<Vec<Item>>;

    /// A pasted link: one item, or the items of a playlist.
    async fn resolve(&self, url: &str) -> Result<Vec<Item>>;

    /// Saves the item as an audio file in `dest_dir` and returns its path.
    /// Dropping the returned future cancels the download.
    async fn download(&self, item: &Item, dest_dir: &Path, progress: ProgressFn) -> Result<PathBuf>;

    /// Like [`download`](Self::download), but may call `ready` with the file path while the
    /// file is still being written, as soon as the start of it can be played. The path is
    /// registered in [`meowtify_core::growth`] until the file is complete. The default
    /// just downloads, and never calls `ready`.
    async fn stream(&self, item: &Item, dest_dir: &Path, progress: ProgressFn, _ready: ReadyFn) -> Result<PathBuf> {
        self.download(item, dest_dir, progress).await
    }

    /// Final touches for a file that [`stream`](Self::stream) saved: tags, and a layout that
    /// seeks well. It may wait a long time, because the file is replaced only when nothing
    /// is reading it (a song that is playing keeps it open). The default does nothing.
    async fn finish(&self, _item: &Item, _path: &Path) -> Result<()> {
        Ok(())
    }

    /// Does the slow lookup for `item` ahead of time, so a later `download` starts at
    /// once. Call it when the user is likely to pick the item. Safe to call many times.
    async fn prefetch(&self, _item: &Item) {}

    /// The results the user was looking at are gone. Lookups from [`prefetch`](Self::prefetch)
    /// that have not started yet are dropped.
    fn cancel_prefetch(&self) {}
}
