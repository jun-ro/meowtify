use async_trait::async_trait;

use crate::error::{Error, Result};
use crate::message::{Place, Selection};
use crate::types::{Page, PlaybackState, Playlist, Queue, Repeat, Track};

/// Something that can play music and report what it is doing.
#[async_trait]
pub trait PlaybackEngine: Send + Sync {
    async fn play(&self) -> Result<()>;
    async fn pause(&self) -> Result<()>;
    async fn next(&self) -> Result<()>;
    async fn previous(&self) -> Result<()>;
    async fn seek(&self, position_ms: u32) -> Result<()>;
    async fn set_volume(&self, percent: u8) -> Result<()>;
    /// Play the queue in random order. Engines that cannot say so.
    async fn set_shuffle(&self, _on: bool) -> Result<()> {
        Err(Error::Unsupported)
    }
    /// Repeat nothing, the whole queue, or the current track.
    async fn set_repeat(&self, _mode: Repeat) -> Result<()> {
        Err(Error::Unsupported)
    }
    /// Puts the song `uri` in the queue.
    async fn enqueue(&self, uri: &str, place: Place) -> Result<()>;
    /// Plays the song `uri`, which waits in the queue, now.
    async fn jump(&self, uri: &str) -> Result<()>;
    async fn play_from(&self, selection: &Selection) -> Result<()>;
    async fn playback_state(&self) -> Result<Option<PlaybackState>>;
    async fn queue(&self) -> Result<Queue>;
}

/// Read access to the user's music.
#[async_trait]
pub trait MetadataSource: Send + Sync {
    async fn user_playlists(&self, offset: u32, limit: u32) -> Result<Page<Playlist>>;
    async fn playlist_items(&self, id: &str, offset: u32, limit: u32) -> Result<Page<Track>>;
    async fn saved_tracks(&self, offset: u32, limit: u32) -> Result<Page<Track>>;
}
