//! In-process providers, for platforms where a subprocess cannot run (iOS).
//!
//! The desktop entry point keeps using yt-dlp; this module is compiled on iOS and with the
//! `native-extract` feature so the same code can be built and tested on a desktop. The HTTP
//! client and the download helpers are always compiled because the Audius provider uses
//! them on every platform.

// Without the feature, only the Audius parts are reachable; the rest waits for iOS.
#![cfg_attr(not(any(target_os = "ios", feature = "native-extract")), allow(dead_code))]

pub(crate) mod fetch;
pub(crate) mod http;

#[cfg(any(target_os = "ios", feature = "native-extract"))]
mod soundcloud;
#[cfg(any(target_os = "ios", feature = "native-extract"))]
mod tags;
#[cfg(any(target_os = "ios", feature = "native-extract"))]
mod youtube;

#[cfg(any(target_os = "ios", feature = "native-extract"))]
pub use soundcloud::NativeSoundCloud;
#[cfg(any(target_os = "ios", feature = "native-extract"))]
pub use youtube::NativeYouTube;

/// The provider set for a platform without subprocesses: YouTube for downloads, with
/// SoundCloud and Audius as fallbacks for songs YouTube does not have.
#[cfg(any(target_os = "ios", feature = "native-extract"))]
pub fn providers() -> (std::sync::Arc<dyn crate::Provider>, Vec<std::sync::Arc<dyn crate::Provider>>) {
    let download: std::sync::Arc<dyn crate::Provider> = std::sync::Arc::new(NativeYouTube::new());
    let fallbacks: Vec<std::sync::Arc<dyn crate::Provider>> =
        vec![std::sync::Arc::new(NativeSoundCloud::new()), std::sync::Arc::new(crate::audius::Audius)];
    (download, fallbacks)
}
