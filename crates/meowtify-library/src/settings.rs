//! User settings, stored as JSON in `%APPDATA%\Meowtify\config.json`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Folder that holds the music. Downloads go here, and it is scanned for files.
    pub library_dir: Option<PathBuf>,
    /// Show the playing song on the user's Discord profile.
    pub discord: bool,
    /// Save data on a metered link (a phone's cellular): downloads use the low bitrate AAC
    /// instead of the best one, and are replaced by the better copy once an unmetered link
    /// (Wi-Fi) is back. Off by default.
    pub low_bitrate: bool,
    /// Start playing a YouTube result while it is still downloading. Off by default.
    pub stream_playback: bool,
    /// When the queue runs out, keep playing songs close to the current one. Off by default.
    pub autoplay: bool,
    /// How many autoplay songs are kept downloaded ahead of the one that plays.
    pub autoplay_ahead: u8,
    /// How many played autoplay songs stay in the cache (for Previous) before they are deleted.
    pub autoplay_behind: u8,
    /// Last.fm API key for better song suggestions. Empty means Last.fm is not used.
    pub lastfm_key: String,
    /// Proxy for every network call, such as `http://127.0.0.1:8080` or `socks5h://host:1080`.
    /// Empty means none. Stored as plain text, so a password in it is readable in this file.
    pub proxy: String,
    /// WebSocket address of the jam relay a hosted jam uses, such as `wss://jam.example.com`.
    /// Empty means no jam can be hosted; joining uses the address in the link.
    pub jam_relay: String,
    /// The name other people in a jam see. Empty means the Windows user name.
    pub jam_name: String,
    /// The colour of the avatar in a jam, in degrees from 0 to 359. `None` takes it from the name.
    pub jam_hue: Option<u16>,
    /// Connect to people in a jam directly, which shows them this computer's IP address. Off by
    /// default: connections then go through the relay's TURN server and the address stays hidden.
    pub jam_share_ip: bool,
}

pub const AHEAD_RANGE: std::ops::RangeInclusive<u8> = 2..=30;
pub const BEHIND_RANGE: std::ops::RangeInclusive<u8> = 0..=20;

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            library_dir: None,
            discord: false,
            low_bitrate: false,
            stream_playback: false,
            autoplay: false,
            autoplay_ahead: 8,
            autoplay_behind: 3,
            lastfm_key: String::new(),
            proxy: String::new(),
            jam_relay: String::new(),
            jam_name: String::new(),
            jam_hue: None,
            jam_share_ip: false,
        }
    }
}

/// `%APPDATA%\Meowtify`, or a `.meowtify` folder in the current directory when
/// `APPDATA` is not set.
pub fn config_dir() -> PathBuf {
    // On iOS the app lives in a sandbox, and `HOME` is its container.
    #[cfg(target_os = "ios")]
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join("Library").join("Application Support").join("Meowtify");
    }
    match std::env::var_os("APPDATA") {
        Some(a) => PathBuf::from(a).join("Meowtify"),
        None => PathBuf::from(".meowtify"),
    }
}

fn path() -> PathBuf {
    config_dir().join("config.json")
}

impl Settings {
    /// Reads the settings file. A missing or damaged file gives the defaults.
    pub fn load() -> Settings {
        std::fs::read(path()).map(|b| Settings::parse(&b)).unwrap_or_default()
    }

    /// Settings from file contents. Anything unreadable gives the defaults. A hand-edited value
    /// out of range is brought into range, so the window and autoplay never disagree about it.
    fn parse(bytes: &[u8]) -> Settings {
        let mut settings: Settings = serde_json::from_slice(bytes).unwrap_or_default();
        settings.autoplay_ahead = settings.autoplay_ahead.clamp(*AHEAD_RANGE.start(), *AHEAD_RANGE.end());
        settings.autoplay_behind = settings.autoplay_behind.clamp(*BEHIND_RANGE.start(), *BEHIND_RANGE.end());
        settings
    }

    /// Changes the saved settings: reads the file, applies `change`, writes it back and returns
    /// the result. Every edit made while the app runs goes through here. Reading again first
    /// means a value that was edited by hand in the meantime is kept, but nothing else is watched:
    /// the app reads the file once at startup and does not follow later edits.
    pub fn update(change: impl FnOnce(&mut Settings)) -> std::io::Result<Settings> {
        let mut settings = Settings::load();
        change(&mut settings);
        settings.save()?;
        Ok(settings)
    }

    /// Writes the file through a temporary file so a crash cannot leave half of it.
    pub fn save(&self) -> std::io::Result<()> {
        let dir = config_dir();
        std::fs::create_dir_all(&dir)?;
        let tmp = dir.join("config.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(&tmp, path())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn out_of_range_autoplay_values_are_brought_into_range() {
        let s = Settings::parse(br#"{"autoplay_ahead": 0, "autoplay_behind": 200}"#);
        assert_eq!((s.autoplay_ahead, s.autoplay_behind), (*AHEAD_RANGE.start(), *BEHIND_RANGE.end()));
        assert_eq!(Settings::parse(b"not json"), Settings::default());
    }

    #[test]
    fn round_trips_and_tolerates_unknown_or_missing_fields() {
        let s = Settings { library_dir: Some(PathBuf::from("D:/Music")), discord: true, low_bitrate: true, stream_playback: true, autoplay: true, autoplay_ahead: 12, autoplay_behind: 1, lastfm_key: "k".into(), proxy: "socks5h://h:1".into(), jam_relay: "wss://j.example".into(), jam_name: "Sleepy Otter".into(), jam_hue: Some(200), jam_share_ip: true };
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<Settings>(&json).unwrap(), s);
        assert_eq!(serde_json::from_str::<Settings>("{}").unwrap(), Settings::default());
        assert!(serde_json::from_str::<Settings>(r#"{"future":1}"#).is_ok());
    }
}
