//! Autoplay: when the queue is about to run out, find songs close to the one that plays,
//! download them ahead of time into a cache folder, and queue them.
//!
//! The listener never waits for a download, because the next songs are fetched while the
//! current one plays. Three parts:
//! - [`rank`] merges what several recommenders suggest (pure, tested),
//! - [`state`] remembers what was played and decides when to search (pure, tested),
//! - [`engine`] does the work: it runs the searches and downloads and sends the queue commands.

mod engine;
mod rank;
mod state;
mod taste;

pub use engine::{cache_dir, cover_link, Autoplay, Control};
