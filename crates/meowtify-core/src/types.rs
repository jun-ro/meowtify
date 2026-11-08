//! Compact domain types. Strings are `Arc<str>` and lists are `Arc<[T]>`, so
//! clones are a refcount bump.

use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artist {
    pub id: Arc<str>,
    pub name: Arc<str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Album {
    pub id: Arc<str>,
    pub name: Arc<str>,
    pub artists: Arc<[Artist]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub id: Arc<str>,
    pub uri: Arc<str>,
    pub name: Arc<str>,
    pub artists: Arc<[Artist]>,
    pub album: Option<Arc<Album>>,
    pub duration_ms: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Playlist {
    pub id: Arc<str>,
    pub name: Arc<str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Repeat {
    #[default]
    Off,
    Context,
    Track,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackState {
    pub is_playing: bool,
    pub progress_ms: u32,
    pub track: Option<Track>,
    /// Output volume, 0 to 100, when the engine knows it.
    pub volume: Option<u8>,
    pub shuffle: bool,
    pub repeat: Repeat,
    /// The queue ran out: `track` is the last song, nothing plays, and Play starts it again.
    /// It stays true until something plays.
    pub ended: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Queue {
    pub current: Option<Track>,
    pub upcoming: Arc<[Track]>,
    /// The first this many `upcoming` songs were queued by the user.
    pub manual: usize,
    /// The next this many songs after those were added by autoplay. The rest come from the
    /// playlist or library that is playing.
    pub auto: usize,
    /// Changes whenever `current`, `upcoming`, `manual` or `auto` change, so a reader can tell
    /// that nothing changed without comparing the songs. Engines that cannot tell leave it 0.
    pub rev: u64,
}

/// One page of a paged listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub offset: u32,
    pub limit: u32,
    pub total: u32,
    pub has_next: bool,
}

impl<T> Page<T> {
    pub fn empty() -> Self {
        Page { items: Vec::new(), offset: 0, limit: 0, total: 0, has_next: false }
    }

    /// Offset to request for the next page, if there is one.
    pub fn next_offset(&self) -> Option<u32> {
        self.has_next.then(|| self.offset + self.items.len() as u32)
    }
}

impl<T> Default for Page<T> {
    fn default() -> Self {
        Self::empty()
    }
}
