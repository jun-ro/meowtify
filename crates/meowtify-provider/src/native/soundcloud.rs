//! SoundCloud in process, with no yt-dlp.
//!
//! The public `api-v2` needs a `client_id` that the web player gets from its JavaScript
//! bundles. This module scrapes one, caches it, and refreshes it when the API refuses a
//! request. Tracks are downloaded as progressive MP3 when the uploader allows it; otherwise
//! the plain (unencrypted) HLS stream is concatenated into an M4A file. FairPlay-protected
//! tracks are refused with a clear message.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::AsyncWriteExt;

use crate::native::{fetch, http};
use crate::soundcloud::validate_url;
use crate::stream::READY_BYTES;
use crate::youtube::url_encode;
use crate::{Item, ProgressFn, Provider, ProviderError, ReadyFn, Result};

const API: &str = "https://api-v2.soundcloud.com";
const HOME: &str = "https://soundcloud.com/";
const MAX_QUERY: usize = 200;
const MAX_SEARCH: u32 = 25;
const CLIENT_TTL: Duration = Duration::from_secs(12 * 60 * 60);
const MIN_AUDIO: u64 = 32 * 1024;
const PAGE: usize = 4 * 1024 * 1024;
const CLIENT_FILE: &str = "soundcloud-client.txt";

/// Where the scraped client id is kept between app launches. iOS gets a Caches folder;
/// desktop gets the user's local cache folder.
fn cache_file() -> Option<PathBuf> {
    let dir = if cfg!(target_os = "ios") {
        PathBuf::from(std::env::var_os("HOME")?).join("Library").join("Caches").join("meowtify")
    } else if let Some(dir) = std::env::var_os("MEOWTIFY_CACHE_DIR") {
        PathBuf::from(dir)
    } else if cfg!(windows) {
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("Meowtify").join("cache")
    } else if let Some(dir) = std::env::var_os("XDG_CACHE_HOME") {
        PathBuf::from(dir).join("meowtify")
    } else {
        PathBuf::from(std::env::var_os("HOME")?).join(".cache").join("meowtify")
    };
    Some(dir.join(CLIENT_FILE))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn cache_line(id: &str, now: u64) -> String {
    format!("{now}\n{id}\n")
}

/// A stored id is used only while it is young enough and still looks like a client id.
fn parse_cache_line(text: &str, now: u64) -> Option<String> {
    let mut lines = text.lines();
    let at: u64 = lines.next()?.trim().parse().ok()?;
    let id = lines.next()?.trim();
    (now.saturating_sub(at) <= CLIENT_TTL.as_secs() && valid_client_id(id)).then(|| id.to_string())
}

fn valid_client_id(id: &str) -> bool {
    (16..=64).contains(&id.len()) && id.chars().all(|c| c.is_ascii_alphanumeric())
}

async fn read_cached_client_id(now: u64) -> Option<String> {
    let path = cache_file()?;
    let text = tokio::fs::read_to_string(path).await.ok()?;
    parse_cache_line(&text, now)
}

async fn write_cached_client_id(id: &str, now: u64) {
    let Some(path) = cache_file() else { return };
    if let Some(dir) = path.parent() {
        let _ = tokio::fs::create_dir_all(dir).await;
    }
    let _ = tokio::fs::write(path, cache_line(id, now)).await;
}

async fn remove_cached_client_id() {
    if let Some(path) = cache_file() {
        let _ = tokio::fs::remove_file(path).await;
    }
}

pub struct NativeSoundCloud {
    tagging: bool,
    client: tokio::sync::Mutex<Option<(String, Instant)>>,
}

impl Default for NativeSoundCloud {
    fn default() -> Self {
        Self::new()
    }
}

impl NativeSoundCloud {
    pub fn new() -> NativeSoundCloud {
        NativeSoundCloud::with_tagging(false)
    }

    pub fn with_tagging(tagging: bool) -> NativeSoundCloud {
        NativeSoundCloud { tagging, client: tokio::sync::Mutex::new(None) }
    }

    async fn client_id(&self) -> Result<String> {
        let mut slot = self.client.lock().await;
        if let Some((id, at)) = slot.as_ref() {
            if at.elapsed() < CLIENT_TTL {
                return Ok(id.clone());
            }
        }
        let now = now_secs();
        if let Some(id) = read_cached_client_id(now).await {
            *slot = Some((id.clone(), Instant::now()));
            return Ok(id);
        }
        let id = scrape_client_id().await?;
        *slot = Some((id.clone(), Instant::now()));
        write_cached_client_id(&id, now).await;
        Ok(id)
    }

    async fn forget_client(&self) {
        *self.client.lock().await = None;
        remove_cached_client_id().await;
    }

    /// GETs an api-v2 URL, retrying once with a fresh `client_id` when it is refused.
    async fn api(&self, make: impl Fn(&str) -> String) -> Result<Value> {
        let id = self.client_id().await?;
        match http::get_json(&make(&id), &[("User-Agent", http::DESKTOP_UA)], PAGE, "SoundCloud").await {
            Ok(value) => Ok(value),
            Err(ProviderError::Tool(message)) if message.contains("refused") => {
                self.forget_client().await;
                let id = self.client_id().await?;
                http::get_json(&make(&id), &[("User-Agent", http::DESKTOP_UA)], PAGE, "SoundCloud").await
            }
            Err(e) => Err(e),
        }
    }

    async fn track(&self, id: &str) -> Result<Value> {
        if !valid_track_id(id) {
            return Err(ProviderError::Invalid("that is not a SoundCloud track".into()));
        }
        self.api(|client| format!("{API}/tracks/{id}?client_id={client}")).await
    }

    /// Picks a stream and adds the `client_id` the CDN asks for.
    async fn choose(&self, id: &str) -> Result<Chosen> {
        let track = self.track(id).await?;
        let transcoding = pick_transcoding(&track)?;
        let client = self.client_id().await?;
        let url = with_client(&transcoding.url, &client);
        Ok(Chosen { url, hls: transcoding.hls, ext: transcoding.ext })
    }

    async fn save_progressive(&self, item: &Item, url: &str, ext: &str, dest_dir: &Path, progress: ProgressFn) -> Result<PathBuf> {
        let path = dest_dir.join(file_name(item, ext));
        let part = path.with_extension(format!("{ext}.part"));
        let guard = fetch::PartFile::new(part.clone());
        let mut attempt = 0;
        let size = loop {
            match fetch::download_plain(url, &[("User-Agent", http::DESKTOP_UA)], &part, progress.clone()).await {
                Ok(size) => break size,
                Err(e) if attempt == 0 && http::transient(&e) => {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(e) => return Err(e),
            }
        };
        if size < MIN_AUDIO {
            return Err(ProviderError::Tool("SoundCloud sent no audio for this track".into()));
        }
        std::fs::rename(&part, &path).map_err(|e| ProviderError::Io(e.to_string()))?;
        guard.keep();
        progress(100.0);
        Ok(path)
    }

    async fn save_hls(&self, item: &Item, url: &str, dest_dir: &Path, progress: ProgressFn) -> Result<PathBuf> {
        let manifest = hls_manifest(url).await?;
        let path = dest_dir.join(file_name(item, "m4a"));
        let part = path.with_extension("m4a.part");
        let guard = fetch::PartFile::new(part.clone());
        let mut file = tokio::fs::File::create(&part).await.map_err(|e| ProviderError::Io(e.to_string()))?;
        if let Some(init) = &manifest.init {
            copy_url_to(init, &mut file).await?;
        }
        let total = manifest.segments.len().max(1);
        for (i, segment) in manifest.segments.iter().enumerate() {
            copy_url_to(segment, &mut file).await?;
            progress(((i + 1) as f64 / total as f64 * 99.0) as f32);
        }
        file.flush().await.map_err(|e| ProviderError::Io(e.to_string()))?;
        drop(file);
        let size = tokio::fs::metadata(&part).await.map(|m| m.len()).unwrap_or(0);
        if size < MIN_AUDIO {
            return Err(ProviderError::Tool("SoundCloud sent no audio for this track".into()));
        }
        std::fs::rename(&part, &path).map_err(|e| ProviderError::Io(e.to_string()))?;
        guard.keep();
        progress(100.0);
        Ok(path)
    }

    async fn stream_progressive(&self, item: &Item, url: &str, ext: &str, dest_dir: &Path, progress: ProgressFn, ready: &ReadyFn) -> Result<PathBuf> {
        use meowtify_core::growth::{self, Growth};
        let path = dest_dir.join(file_name(item, ext));
        let growth = Growth::new();
        growth::register(&path, growth.clone());
        let mut guard = Growing { path: path.clone(), growth: growth.clone(), armed: true };
        let mut reply = http::get(url, &[("User-Agent", http::DESKTOP_UA)]).await?;
        if reply.status != 200 {
            let status = reply.status;
            let body = reply.bytes(16 * 1024).await.unwrap_or_default();
            return Err(http::status_error("SoundCloud", status, &body));
        }
        let total = reply.content_length();
        let mut file = tokio::fs::File::create(&path).await.map_err(|e| ProviderError::Io(e.to_string()))?;
        let mut announced = false;
        let mut written = 0u64;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = match tokio::time::timeout(Duration::from_secs(120), reply.read(&mut buf)).await {
                Ok(r) => r?,
                Err(_) => return Err(ProviderError::Tool("the download timed out".into())),
            };
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n]).await.map_err(|e| ProviderError::Io(e.to_string()))?;
            written += n as u64;
            growth.set_written(written);
            if let Some(total) = total.filter(|t| *t > 0) {
                progress((written as f64 / total as f64 * 100.0).min(99.0) as f32);
            }
            if !announced && written >= READY_BYTES {
                announced = true;
                ready(&path);
            }
        }
        file.flush().await.map_err(|e| ProviderError::Io(e.to_string()))?;
        drop(file);
        if written < MIN_AUDIO {
            return Err(ProviderError::Tool("SoundCloud sent no audio for this track".into()));
        }
        growth.finish(written);
        guard.armed = false;
        growth::forget(&path);
        if !announced {
            ready(&path);
        }
        progress(100.0);
        Ok(path)
    }
}

struct Chosen {
    url: String,
    hls: bool,
    ext: &'static str,
}

struct Growing {
    path: PathBuf,
    growth: Arc<meowtify_core::growth::Growth>,
    armed: bool,
}

impl Drop for Growing {
    fn drop(&mut self) {
        if self.armed {
            self.growth.fail();
            meowtify_core::growth::forget(&self.path);
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[async_trait]
impl Provider for NativeSoundCloud {
    async fn search(&self, query: &str, limit: u32) -> Result<Vec<Item>> {
        let q = query.trim();
        if q.is_empty() || q.chars().count() > MAX_QUERY || q.chars().any(|c| c.is_control()) {
            return Err(ProviderError::Invalid("enter a search of 1 to 200 characters".into()));
        }
        let n = limit.clamp(1, MAX_SEARCH);
        let answer = self
            .api(|client| {
                format!(
                    "{API}/search/tracks?q={}&client_id={client}&limit={n}&offset=0&linked_partitioning=1&app_locale=en",
                    url_encode(q)
                )
            })
            .await?;
        Ok(parse_search(&answer, n as usize))
    }

    async fn resolve(&self, url: &str) -> Result<Vec<Item>> {
        let url = validate_url(url)?;
        let answer = self.api(|client| format!("{API}/resolve?url={}&client_id={client}", url_encode(&url))).await?;
        let items = parse_resolve(&answer);
        if items.is_empty() {
            return Err(ProviderError::Tool("SoundCloud did not list anything at that link".into()));
        }
        Ok(items)
    }

    async fn stream(&self, item: &Item, dest_dir: &Path, progress: ProgressFn, ready: ReadyFn) -> Result<PathBuf> {
        std::fs::create_dir_all(dest_dir).map_err(|e| ProviderError::Io(e.to_string()))?;
        match self.choose(&item.id).await? {
            Chosen { url, hls: false, ext } => self.stream_progressive(item, &url, ext, dest_dir, progress, &ready).await,
            Chosen { url, hls: true, .. } => {
                let path = self.save_hls(item, &url, dest_dir, progress).await?;
                ready(&path);
                Ok(path)
            }
        }
    }

    async fn finish(&self, item: &Item, path: &Path) -> Result<()> {
        if !self.tagging {
            return Ok(());
        }
        crate::native::tags::tag_copy(path, item).await
    }

    async fn download(&self, item: &Item, dest_dir: &Path, progress: ProgressFn) -> Result<PathBuf> {
        std::fs::create_dir_all(dest_dir).map_err(|e| ProviderError::Io(e.to_string()))?;
        let path = match self.choose(&item.id).await? {
            Chosen { url, hls: false, ext } => self.save_progressive(item, &url, ext, dest_dir, progress).await?,
            Chosen { url, hls: true, .. } => self.save_hls(item, &url, dest_dir, progress).await?,
        };
        // Title and artist, so the library lists the artist instead of "Unknown artist".
        let _ = self.finish(item, &path).await;
        Ok(path)
    }
}

fn with_client(url: &str, client: &str) -> String {
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}client_id={client}")
}

/// The track id is numeric on SoundCloud. Anything else is refused before a request.
fn valid_track_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 20 && id.chars().all(|c| c.is_ascii_digit())
}

/// `Title [id].ext`, in the library's style. Characters Windows forbids become `_`.
fn file_name(item: &Item, ext: &str) -> String {
    let mut title: String = item
        .title
        .chars()
        .map(|c| if c.is_control() || "<>:\"/\\|?*".contains(c) { '_' } else { c })
        .take(120)
        .collect();
    title = title.trim().trim_end_matches('.').trim().to_string();
    if title.is_empty() {
        title = "Unknown".to_string();
    }
    format!("{title} [{}].{ext}", item.id)
}

fn track_item(track: &Value) -> Option<Item> {
    if track.get("streamable").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    if track.get("policy").and_then(Value::as_str) == Some("BLOCK") {
        return None;
    }
    if track.get("access").and_then(Value::as_str) == Some("blocked") {
        return None;
    }
    let id = track
        .get("id")
        .and_then(|v| v.as_str().map(str::to_string).or_else(|| v.as_u64().map(|n| n.to_string())))?;
    if !valid_track_id(&id) {
        return None;
    }
    let title = track.get("title").and_then(Value::as_str).filter(|t| !t.is_empty())?.to_string();
    let artist = track.pointer("/user/username").and_then(Value::as_str).unwrap_or("").to_string();
    let duration_secs = track.get("duration").and_then(Value::as_u64).map(|ms| (ms / 1000) as u32);
    let url = track
        .get("permalink_url")
        .and_then(Value::as_str)
        .filter(|u| validate_url(u).is_ok())
        .map(str::to_string)
        .unwrap_or_else(|| format!("https://soundcloud.com/tracks/{id}"));
    let thumbnail = track
        .get("artwork_url")
        .and_then(Value::as_str)
        .map(|u| u.replace("-large", "-t500x500"));
    Some(Item { provider: "soundcloud", id, title, artist, duration_secs, url, thumbnail })
}

fn parse_search(answer: &Value, limit: usize) -> Vec<Item> {
    let rows = answer.get("collection").and_then(Value::as_array);
    rows.into_iter()
        .flatten()
        .filter(|row| row.get("kind").and_then(Value::as_str).is_none_or(|k| k == "track"))
        .filter_map(track_item)
        .take(limit)
        .collect()
}

fn parse_resolve(answer: &Value) -> Vec<Item> {
    match answer.get("kind").and_then(Value::as_str) {
        Some("playlist") => answer
            .get("tracks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(track_item)
            .take(200)
            .collect(),
        _ => track_item(answer).into_iter().collect(),
    }
}

struct Transcoding {
    url: String,
    hls: bool,
    ext: &'static str,
}

/// Picks the best plain stream. DRM manifests (`SAMPLE-AES`, `skd://`) are never used.
fn pick_transcoding(track: &Value) -> Result<Transcoding> {
    let list = track.pointer("/media/transcodings").and_then(Value::as_array);
    let mut best: Option<(i32, Transcoding)> = None;
    let mut drm = false;
    for item in list.into_iter().flatten() {
        let url = item.get("url").and_then(Value::as_str).unwrap_or("");
        let protocol = item.pointer("/format/protocol").and_then(Value::as_str).unwrap_or("");
        let mime = item.pointer("/format/mime_type").and_then(Value::as_str).unwrap_or("");
        if protocol.contains("encrypted") || protocol.contains("ctr-enc") {
            drm = true;
            continue;
        }
        if !url.starts_with("https://") || url.chars().any(|c| c.is_control()) {
            continue;
        }
        let (score, hls, ext) = if protocol == "progressive" && mime.contains("mpeg") {
            (100, false, "mp3")
        } else if protocol == "progressive" && mime.contains("mp4") {
            (95, false, "m4a")
        } else if protocol == "hls" && mime.contains("mp4") {
            (80, true, "m4a")
        } else if protocol == "hls" && mime.contains("mpeg") {
            (75, true, "m4a")
        } else {
            continue;
        };
        if best.as_ref().is_none_or(|(s, _)| score > *s) {
            best = Some((score, Transcoding { url: url.to_string(), hls, ext }));
        }
    }
    match best {
        Some((_, chosen)) => Ok(chosen),
        None if drm => Err(ProviderError::Tool(
            "this SoundCloud track is DRM-protected and cannot be downloaded".into(),
        )),
        None => Err(ProviderError::Tool("SoundCloud has no downloadable audio for this track".into())),
    }
}

/// A media playlist. `init` is the fMP4 header (`EXT-X-MAP`) when there is one.
struct Hls {
    init: Option<String>,
    segments: Vec<String>,
}

/// Reads a media playlist and resolves its relative links against `base`.
fn parse_hls(text: &str, base: &str) -> Result<Hls> {
    if !text.trim_start().starts_with("#EXTM3U") {
        return Err(ProviderError::Parse("SoundCloud sent an unreadable audio list".into()));
    }
    let base_url = http::Url::parse(base)?;
    let mut init = None;
    let mut segments = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-KEY:") {
            if !rest.contains("METHOD=NONE") {
                return Err(ProviderError::Tool(
                    "this SoundCloud track is DRM-protected and cannot be downloaded".into(),
                ));
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-MAP:") {
            if let Some(uri) = attribute(rest, "URI") {
                init = Some(http::resolve_location(&base_url, &uri)?);
            }
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        segments.push(http::resolve_location(&base_url, line)?);
    }
    if segments.is_empty() {
        return Err(ProviderError::Parse("SoundCloud's audio list is empty".into()));
    }
    Ok(Hls { init, segments })
}

fn attribute(text: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = text.find(&needle)? + needle.len();
    let rest = &text[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// The HLS answer of a transcoding is a small JSON object with the manifest link.
async fn hls_manifest(transcoding: &str) -> Result<Hls> {
    let mut reply = http::get(transcoding, &[("User-Agent", http::DESKTOP_UA)]).await?;
    let status = reply.status;
    let body = reply.bytes(256 * 1024).await?;
    if status != 200 {
        return Err(http::status_error("SoundCloud", status, &body));
    }
    let text = String::from_utf8_lossy(&body);
    if text.trim_start().starts_with("#EXTM3U") {
        return parse_hls(&text, transcoding);
    }
    let answer: Value = serde_json::from_slice(&body)
        .map_err(|_| ProviderError::Parse("SoundCloud did not send an audio list".into()))?;
    let url = answer
        .get("url")
        .and_then(Value::as_str)
        .filter(|u| u.starts_with("https://"))
        .ok_or_else(|| ProviderError::Parse("SoundCloud's audio list has no link".into()))?;
    let mut reply = http::get(url, &[("User-Agent", http::DESKTOP_UA)]).await?;
    let status = reply.status;
    let text = String::from_utf8_lossy(&reply.bytes(2 * 1024 * 1024).await?).into_owned();
    if status != 200 {
        return Err(ProviderError::Tool(format!("SoundCloud's audio list answered {status}")));
    }
    parse_hls(&text, url)
}

async fn copy_url_to(url: &str, file: &mut tokio::fs::File) -> Result<u64> {
    let mut reply = http::get(url, &[("User-Agent", http::DESKTOP_UA)]).await?;
    match reply.status {
        200 | 206 => {}
        status => {
            let body = reply.bytes(16 * 1024).await.unwrap_or_default();
            return Err(http::status_error("SoundCloud's audio server", status, &body));
        }
    }
    fetch::copy_body(&mut reply, file, |_| {}).await
}

/// Finds the web player's `client_id` in the JavaScript bundles the home page loads.
async fn scrape_client_id() -> Result<String> {
    let mut page = http::get(HOME, &[("User-Agent", http::DESKTOP_UA), ("Accept", "text/html")]).await?;
    if page.status != 200 {
        return Err(ProviderError::Tool(format!("SoundCloud's home page answered {}", page.status)));
    }
    let html = String::from_utf8_lossy(&page.bytes(3 * 1024 * 1024).await?).into_owned();
    let mut seen = Vec::new();
    for src in script_urls(&html) {
        if seen.contains(&src) {
            continue;
        }
        seen.push(src.clone());
        if seen.len() > 20 {
            break;
        }
        let mut reply = match http::get(&src, &[("User-Agent", http::DESKTOP_UA)]).await {
            Ok(reply) => reply,
            Err(_) => continue,
        };
        if reply.status != 200 {
            continue;
        }
        let js = reply.bytes(8 * 1024 * 1024).await.unwrap_or_default();
        if let Some(id) = find_client_id(&js) {
            return Ok(id);
        }
    }
    Err(ProviderError::Tool("SoundCloud did not give this app a client id".into()))
}

fn script_urls(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(at) = rest.find("src=\"") {
        let after = &rest[at + 5..];
        let Some(end) = after.find('"') else { break };
        let url = &after[..end];
        if url.starts_with("https://") && url.ends_with(".js") && url.contains("sndcdn.com") {
            out.push(url.to_string());
        }
        rest = &after[end + 1..];
    }
    out
}

fn find_client_id(js: &[u8]) -> Option<String> {
    let needle = b"client_id";
    let mut from = 0;
    while from + needle.len() <= js.len() {
        let at = js[from..].windows(needle.len()).position(|w| w == needle)? + from;
        let mut i = at + needle.len();
        while i < js.len() && matches!(js[i], b':' | b'=' | b'"' | b'\'' | b' ' | b'\t' | b'\\') {
            i += 1;
        }
        let start = i;
        while i < js.len() && js[i].is_ascii_alphanumeric() {
            i += 1;
        }
        let value = &js[start..i];
        if (16..=64).contains(&value.len()) {
            return Some(String::from_utf8_lossy(value).into_owned());
        }
        from = at + needle.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track() -> Value {
        serde_json::json!({
            "kind": "track",
            "id": 718846078,
            "title": "Blinding Lights",
            "duration": 201619,
            "permalink_url": "https://soundcloud.com/theweeknd/blinding-lights",
            "streamable": true,
            "user": { "username": "The Weeknd" },
            "media": { "transcodings": [
                { "url": "https://api-v2.soundcloud.com/media/x/hls", "format": { "protocol": "ctr-encrypted-hls", "mime_type": "audio/mp4" } },
                { "url": "https://api-v2.soundcloud.com/media/x/progressive", "format": { "protocol": "progressive", "mime_type": "audio/mpeg" } }
            ] }
        })
    }

    #[test]
    fn reads_search_answers() {
        let answer = serde_json::json!({ "collection": [
            track(),
            { "kind": "playlist", "id": 1, "title": "nope" },
            { "kind": "track", "id": 2, "title": "blocked", "streamable": false },
            { "kind": "track", "id": 3, "title": "no page" }
        ] });
        let items = parse_search(&answer, 10);
        assert_eq!(items.len(), 2);
        assert_eq!((items[0].id.as_str(), items[0].artist.as_str(), items[0].duration_secs), ("718846078", "The Weeknd", Some(201)));
        assert_eq!(items[0].url, "https://soundcloud.com/theweeknd/blinding-lights");
        assert_eq!(items[1].url, "https://soundcloud.com/tracks/3");
    }

    #[test]
    fn prefers_plain_progressive_and_refuses_drm() {
        let chosen = pick_transcoding(&track()).unwrap();
        assert_eq!((chosen.hls, chosen.ext), (false, "mp3"));
        let only_drm = serde_json::json!({ "media": { "transcodings": [
            { "url": "https://api-v2.soundcloud.com/media/x/hls", "format": { "protocol": "ctr-encrypted-hls", "mime_type": "audio/mp4" } }
        ] } });
        assert!(pick_transcoding(&only_drm).is_err());
    }

    #[test]
    fn reads_a_plain_hls_list() {
        let text = "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4,\nseg1.m4s\n#EXTINF:4,\nhttps://cdn.example/seg2.m4s\n#EXT-X-ENDLIST\n";
        let hls = parse_hls(text, "https://cdn.example/audio/index.m3u8").unwrap();
        assert_eq!(hls.init.as_deref(), Some("https://cdn.example/audio/init.mp4"));
        assert_eq!(hls.segments, vec!["https://cdn.example/audio/seg1.m4s", "https://cdn.example/seg2.m4s"]);
    }

    #[test]
    fn refuses_encrypted_lists() {
        let text = "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://x\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n#EXTINF:4,\nseg.m4s\n";
        assert!(parse_hls(text, "https://cdn.example/i.m3u8").is_err());
        assert!(parse_hls("not a list", "https://cdn.example/i.m3u8").is_err());
        assert!(parse_hls("#EXTM3U\n", "https://cdn.example/i.m3u8").is_err());
    }

    #[test]
    fn finds_client_ids_in_bundles() {
        assert_eq!(find_client_id(br#"var x = {client_id:"39AVoiZVrdvJEMFLH8T0c5WGshQaAPRJ"};"#).as_deref(), Some("39AVoiZVrdvJEMFLH8T0c5WGshQaAPRJ"));
        assert_eq!(find_client_id(br#"client_id=abc"#), None);
        assert_eq!(find_client_id(b"nothing here"), None);
    }

    #[test]
    fn reads_script_urls() {
        let html = r#"<script crossorigin src="https://a-v2.sndcdn.com/assets/50-abc.js"></script><script src="https://evil.example/x.js"></script>"#;
        assert_eq!(script_urls(html), vec!["https://a-v2.sndcdn.com/assets/50-abc.js"]);
    }

    #[test]
    fn names_files_in_the_library_style() {
        let item = Item {
            provider: "soundcloud",
            id: "718846078".into(),
            title: "AC/DC: Back <in> Black?".into(),
            artist: String::new(),
            duration_secs: None,
            url: String::new(),
            thumbnail: None,
        };
        assert_eq!(file_name(&item, "mp3"), "AC_DC_ Back _in_ Black_ [718846078].mp3");
        assert!(valid_track_id("718846078") && !valid_track_id("../x"));
    }

    #[test]
    fn reads_and_expires_a_cached_client_id() {
        let now = 1_700_000_000;
        let text = cache_line("39AVoiZVrdvJEMFLH8T0c5WGshQaAPRJ", now);
        assert_eq!(parse_cache_line(&text, now).as_deref(), Some("39AVoiZVrdvJEMFLH8T0c5WGshQaAPRJ"));
        assert_eq!(parse_cache_line(&text, now + CLIENT_TTL.as_secs() + 1), None);
        assert_eq!(parse_cache_line("nope", now), None);
        assert_eq!(parse_cache_line(&format!("{now}\nshort\n"), now), None);
        assert!(valid_client_id("39AVoiZVrdvJEMFLH8T0c5WGshQaAPRJ"));
    }

    /// Needs the network: `cargo test -p meowtify-provider --features native-extract live_native_sc -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_native_sc_search_and_download() {
        let sc = NativeSoundCloud::new();
        let items = sc.search("blinding lights", 5).await.unwrap();
        eprintln!("{} results, first {:?}", items.len(), items.first());
        assert!(!items.is_empty());
        let dir = std::env::temp_dir().join(format!("meowtify-native-sc-{}", std::process::id()));
        let path = sc.download(&items[0], &dir, Arc::new(|p| eprintln!("{p:.0}%"))).await.unwrap();
        eprintln!("saved {} ({} bytes)", path.display(), std::fs::metadata(&path).unwrap().len());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
