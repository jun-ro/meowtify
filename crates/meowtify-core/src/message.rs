use std::sync::Arc;

use crate::error::Error;
use crate::types::{PlaybackState, Queue, Repeat};

/// Where a song put in the queue goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// Right after the loaded song, before everything queued.
    Next,
    /// After the songs the user queued, before autoplay songs.
    Queue,
    /// After the songs the user queued and the autoplay songs already waiting.
    Auto,
}

/// What to play, and so what the queue becomes. Songs are named by their `Track::uri`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// The song, with the rest of the library after it.
    Library(Arc<str>),
    /// The song alone, so autoplay can carry on from it.
    Single(Arc<str>),
    /// A playlist, from its first song or from one of its songs.
    Playlist { id: Arc<str>, from: Option<Arc<str>> },
}

/// Requests sent into the core. Flow is one way: commands in, events out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Play,
    Pause,
    Next,
    Previous,
    Seek(u32),
    SetVolume(u8),
    SetShuffle(bool),
    SetRepeat(Repeat),
    /// Puts a song in the queue.
    Enqueue { uri: Arc<str>, place: Place },
    /// Plays a song that already waits in the queue, now. The songs before it are passed
    /// over; Previous returns to them.
    Jump(Arc<str>),
    PlayFrom(Selection),
    Refresh,
}

/// Facts published by the core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    PlaybackChanged(Option<PlaybackState>),
    QueueChanged(Queue),
    Error(Error),
}
