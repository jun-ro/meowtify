//! Meowtify core: domain types, the command and event vocabulary, the engine
//! and metadata traits, and the app actor that ties them together.
//!
//! This crate has no network or UI dependencies. Flow is one way: callers send
//! a [`Command`], the actor calls the [`PlaybackEngine`], and results come back
//! as [`Event`] values on a broadcast channel.

mod actor;
mod error;
pub mod growth;
mod ids;
mod message;
pub mod quality;
mod traits;
mod types;

pub use actor::{run, AppHandle};
pub use error::{Error, Result};
pub use ids::{cover_id, download_id, set_cover_id, youtube_id, youtube_thumbnail};
pub use message::{Command, Event, Place, Selection};
pub use traits::{MetadataSource, PlaybackEngine};
pub use types::{Album, Artist, Page, PlaybackState, Playlist, Queue, Repeat, Track};
