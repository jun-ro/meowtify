//! Audius: music uploaded by independent artists, with a public API that needs no key and
//! streams plain MP3 files. No yt-dlp or ffmpeg involved, and no subprocess either: the
//! request goes through the in-process HTTP client so the same code runs on iOS.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::Value;

use crate::native::{fetch, http};
use crate::youtube::url_encode;
use crate::{Item, ProgressFn, Provider, ProviderError, Result};

const API: &str = "https://api.audius.co/v1";
const APP: &str = "Meowtify";
const MAX_QUERY: usize = 200;
const MAX_SEARCH: u32 = 25;
const MIN_AUDIO: u64 = 32 * 1024;

pub struct Audius;

#[async_trait]
impl Provider for Audius {
    async fn search(&self, query: &str, limit: u32) -> Result<Vec<Item>> {
        let q = query.trim();
        if q.is_empty() || q.chars().count() > MAX_QUERY || q.chars().any(|c| c.is_control()) {
            return Err(ProviderError::Invalid("enter a search of 1 to 200 characters".into()));
        }
        let n = limit.clamp(1, MAX_SEARCH);
        let url = format!("{API}/tracks/search?query={}&limit={n}&app_name={APP}", url_encode(q));
        let answer = http::get_json(&url, &[("User-Agent", http::DESKTOP_UA)], 4 * 1024 * 1024, "Audius").await?;
        Ok(parse_search_value(&answer))
    }

    async fn resolve(&self, _url: &str) -> Result<Vec<Item>> {
        Err(ProviderError::Invalid("pasting Audius links is not supported; search instead".into()))
    }

    async fn download(&self, item: &Item, dest_dir: &Path, progress: ProgressFn) -> Result<PathBuf> {
        if !valid_id(&item.id) {
            return Err(ProviderError::Invalid("that is not an Audius track".into()));
        }
        std::fs::create_dir_all(dest_dir).map_err(|e| ProviderError::Io(e.to_string()))?;
        let dest = dest_dir.join(file_name(item));
        let part = dest.with_extension("mp3.part");
        let guard = fetch::PartFile::new(part.clone());
        let url = format!("{API}/tracks/{}/stream?app_name={APP}", item.id);
        let mut attempt = 0;
        let len = loop {
            match fetch::download_plain(&url, &[("User-Agent", http::DESKTOP_UA)], &part, progress.clone()).await {
                Ok(len) => break len,
                Err(e) if attempt == 0 && http::transient(&e) => {
                    attempt += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                Err(e) => return Err(e),
            }
        };
        // A short file is an error page or a cut connection, not a song.
        if len < MIN_AUDIO {
            return Err(ProviderError::Tool("Audius sent no audio for this track".into()));
        }
        std::fs::rename(&part, &dest).map_err(|e| ProviderError::Io(e.to_string()))?;
        guard.keep();
        progress(100.0);
        Ok(dest)
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 16 && id.chars().all(|c| c.is_ascii_alphanumeric())
}

/// `Title [id].mp3` with characters that Windows file names cannot hold removed, and a
/// length limit. The `[id].` part is how the library finds a downloaded song again.
fn file_name(item: &Item) -> String {
    let title: String = item
        .title
        .chars()
        .map(|c| if c.is_control() || "<>:\"/\\|?*".contains(c) { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let title: String = title.chars().take(120).collect();
    let title = title.trim().trim_end_matches('.');
    let title = if title.is_empty() { "track" } else { title };
    format!("{title} [{}].mp3", item.id)
}

#[cfg(test)]
fn parse_search(body: &[u8]) -> Result<Vec<Item>> {
    let v: Value = serde_json::from_slice(body).map_err(|e| ProviderError::Parse(e.to_string()))?;
    let rows = v.get("data").and_then(Value::as_array).ok_or_else(|| ProviderError::Parse("no data".into()))?;
    Ok(rows.iter().filter_map(item).collect())
}

fn parse_search_value(v: &Value) -> Vec<Item> {
    v.get("data").and_then(Value::as_array).into_iter().flatten().filter_map(item).collect()
}

fn item(r: &Value) -> Option<Item> {
    let id = r.get("id")?.as_str().filter(|i| valid_id(i))?.to_string();
    let title = r.get("title")?.as_str()?.to_string();
    // Tracks that cannot be streamed (blocked, or removed) have no use here.
    if r.get("is_streamable").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    let artist = r.pointer("/user/name").and_then(Value::as_str).unwrap_or("").to_string();
    let permalink = r.get("permalink").and_then(Value::as_str).unwrap_or("");
    Some(Item {
        provider: "audius",
        url: format!("https://audius.co{permalink}"),
        duration_secs: r.get("duration").and_then(Value::as_u64).map(|d| d as u32),
        thumbnail: ["480x480", "150x150", "1000x1000"]
            .iter()
            .find_map(|size| r.pointer(&format!("/artwork/{size}")).and_then(Value::as_str))
            .filter(|u| u.starts_with("https://"))
            .map(str::to_string),
        id,
        title,
        artist,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_artwork_link_is_read_when_it_is_https() {
        let body = br#"{"data":[
            {"id":"A1","title":"a","artwork":{"150x150":"https://n.audius.co/a/150x150.jpg","480x480":"https://n.audius.co/a/480x480.jpg"}},
            {"id":"A2","title":"b","artwork":{"150x150":"https://n.audius.co/b/150x150.jpg"}},
            {"id":"A3","title":"c","artwork":{"480x480":"http://n.audius.co/c.jpg"}},
            {"id":"A4","title":"d"}]}"#;
        let got = parse_search(body).unwrap();
        assert_eq!(got[0].thumbnail.as_deref(), Some("https://n.audius.co/a/480x480.jpg"));
        assert_eq!(got[1].thumbnail.as_deref(), Some("https://n.audius.co/b/150x150.jpg"));
        assert_eq!(got[2].thumbnail, None);
        assert_eq!(got[3].thumbnail, None);
    }

    #[test]
    fn reads_search_answers() {
        let body = br#"{"data":[
            {"id":"5KZ2E72","title":"Blinding Lights - Zaza Remix","duration":227,"permalink":"/zaza/blinding-lights","user":{"name":"Zaza"}},
            {"id":"bad id!","title":"x"},
            {"id":"BLOCKED1","title":"y","is_streamable":false},
            {"id":"NoUser1","title":"Plain","duration":100}]}"#;
        let got = parse_search(body).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!((got[0].provider, got[0].artist.as_str(), got[0].duration_secs), ("audius", "Zaza", Some(227)));
        assert_eq!(got[0].url, "https://audius.co/zaza/blinding-lights");
        assert_eq!(got[1].artist, "");
        assert!(parse_search(b"{}").is_err());
    }

    /// Needs the network: searches, then saves the first result.
    #[tokio::test]
    #[ignore]
    async fn live_search_and_download() {
        let items = Audius.search("blinding lights", 5).await.unwrap();
        assert!(!items.is_empty());
        let dir = std::env::temp_dir().join(format!("meowtify-audius-{}", std::process::id()));
        let started = std::time::Instant::now();
        let path = Audius.download(&items[0], &dir, std::sync::Arc::new(|_| {})).await.unwrap();
        println!("{} bytes in {:?}: {}", std::fs::metadata(&path).unwrap().len(), started.elapsed(), path.display());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_names_are_safe_and_carry_the_id() {
        let it = |t: &str| Item { provider: "audius", id: "5KZ2E72".into(), title: t.into(), artist: String::new(), duration_secs: None, url: String::new(), thumbnail: None };
        assert_eq!(file_name(&it("a/b:c*d?")), "a b c d [5KZ2E72].mp3");
        assert_eq!(file_name(&it("...")), "track [5KZ2E72].mp3");
        assert!(file_name(&it(&"x".repeat(500))).len() < 150);
        assert!(!valid_id("../x") && valid_id("5KZ2E72"));
    }
}
