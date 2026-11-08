//! SoundCloud, through yt-dlp. It has many remixes, bootlegs and independent songs that
//! YouTube Music does not list, so it works as a second source when a song is missing there.
//!
//! It shares the programs and the download code of [`YouTube`]; only search, the link check
//! and the reading of results differ.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;

use crate::youtube::{Tools, YouTube};
use crate::{Item, ProgressFn, Provider, ProviderError, Result};

const MAX_QUERY: usize = 200;
const MAX_SEARCH: u32 = 25;
const HOSTS: [&str; 3] = ["soundcloud.com", "www.soundcloud.com", "m.soundcloud.com"];

pub struct SoundCloud {
    inner: YouTube,
}

impl SoundCloud {
    /// Shares the yt-dlp of `youtube`, so an update made from Settings applies here too.
    pub fn new(youtube: YouTube) -> SoundCloud {
        SoundCloud { inner: youtube }
    }

    /// For tests and tools that have no `YouTube` yet.
    pub fn from_tools(tools: Tools) -> SoundCloud {
        SoundCloud { inner: YouTube::new(tools) }
    }
}

#[derive(Deserialize)]
struct Entry {
    id: Option<serde_json::Value>,
    title: Option<String>,
    uploader: Option<String>,
    duration: Option<f64>,
    webpage_url: Option<String>,
    url: Option<String>,
}

/// One line of yt-dlp JSON from a SoundCloud listing. Entries without an id or a page are skipped.
fn parse_entry(line: &str) -> Result<Option<Item>> {
    let e: Entry = serde_json::from_str(line).map_err(|e| ProviderError::Parse(e.to_string()))?;
    let id = match e.id {
        Some(serde_json::Value::String(s)) if !s.is_empty() => s,
        Some(serde_json::Value::Number(n)) => n.to_string(),
        _ => return Ok(None),
    };
    let Some(url) = e.webpage_url.or(e.url).filter(|u| validate_url(u).is_ok()) else { return Ok(None) };
    Ok(Some(Item {
        provider: "soundcloud",
        title: e.title.unwrap_or_else(|| id.clone()),
        artist: e.uploader.unwrap_or_default(),
        duration_secs: e.duration.filter(|d| *d >= 0.0).map(|d| d as u32),
        url,
        thumbnail: None,
        id,
    }))
}

/// Accepts https links to soundcloud.com only, so a result cannot point yt-dlp elsewhere.
fn validate_url(s: &str) -> Result<String> {
    let bad = |m: &str| Err(ProviderError::Invalid(m.to_string()));
    let s = s.trim();
    if s.len() > 500 || s.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return bad("that is not a SoundCloud link");
    }
    let Some(rest) = s.strip_prefix("https://") else { return bad("only https SoundCloud links are supported") };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("").to_ascii_lowercase();
    if !HOSTS.contains(&host.as_str()) {
        return bad("only SoundCloud links are supported");
    }
    Ok(s.to_string())
}

#[async_trait]
impl Provider for SoundCloud {
    async fn search(&self, query: &str, limit: u32) -> Result<Vec<Item>> {
        let q = query.trim();
        if q.is_empty() || q.chars().count() > MAX_QUERY || q.chars().any(|c| c.is_control()) {
            return Err(ProviderError::Invalid("enter a search of 1 to 200 characters".into()));
        }
        let n = limit.clamp(1, MAX_SEARCH);
        let mut cmd = self.inner.command();
        cmd.args(["--flat-playlist", "--dump-json", "--skip-download", "--"]);
        cmd.arg(format!("scsearch{n}:{q}"));
        self.inner.list_with(cmd, parse_entry).await
    }

    async fn resolve(&self, url: &str) -> Result<Vec<Item>> {
        let url = validate_url(url)?;
        let mut cmd = self.inner.command();
        cmd.args(["--flat-playlist", "--dump-json", "--skip-download", "--playlist-end", "200", "--"]);
        cmd.arg(url);
        self.inner.list_with(cmd, parse_entry).await
    }

    async fn download(&self, item: &Item, dest_dir: &Path, progress: ProgressFn) -> Result<PathBuf> {
        let url = validate_url(&item.url)?;
        std::fs::create_dir_all(dest_dir).map_err(|e| ProviderError::Io(e.to_string()))?;
        self.inner.run_download(item, &url, dest_dir, progress, None).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_listing_lines() {
        let line = r#"{"id":"718846078","title":"Blinding Lights","uploader":"The Weeknd","duration":201.619,"url":"https://api.soundcloud.com/tracks/soundcloud%3Atracks%3A718846078","webpage_url":"https://soundcloud.com/theweeknd/blinding-lights"}"#;
        let it = parse_entry(line).unwrap().unwrap();
        assert_eq!((it.provider, it.id.as_str(), it.artist.as_str(), it.duration_secs), ("soundcloud", "718846078", "The Weeknd", Some(201)));
        assert_eq!(it.url, "https://soundcloud.com/theweeknd/blinding-lights");
        // A numeric id works, a missing page or id skips the row.
        assert!(parse_entry(r#"{"id":5,"webpage_url":"https://soundcloud.com/a/b"}"#).unwrap().is_some());
        assert!(parse_entry(r#"{"id":5,"webpage_url":"https://evil.example/a"}"#).unwrap().is_none());
        assert!(parse_entry(r#"{"title":"x","webpage_url":"https://soundcloud.com/a/b"}"#).unwrap().is_none());
    }

    #[test]
    fn accepts_soundcloud_links_only() {
        assert!(validate_url("https://soundcloud.com/a/b").is_ok());
        assert!(validate_url("https://m.soundcloud.com/a/b?x=1").is_ok());
        for bad in ["http://soundcloud.com/a", "https://soundcloud.com.evil.example/a", "https://youtube.com/watch?v=x", "https://soundcloud.com/a b", "--exec"] {
            assert!(validate_url(bad).is_err(), "{bad}");
        }
    }
}
