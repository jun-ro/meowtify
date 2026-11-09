//! YouTube in process, with no yt-dlp.
//!
//! Search uses the same YouTube Music endpoint the desktop fast path uses; the player
//! endpoint supplies a direct `googlevideo.com` link for the AAC stream. Only formats the
//! player can decode are chosen (MP4/AAC, or MP3), and only `https` links are fetched.
//!
//! The mobile clients (visionOS, Android VR, iOS) are the ones that still answer without a
//! signed-in browser session, and they need a visitor id. It is fetched once from an
//! Innertube search answer and sent with every player request. Several clients are tried in
//! turn because YouTube changes which one works.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

use crate::native::{fetch, http};
use crate::stream::READY_BYTES;
use crate::youtube::{merge, parse_clock, parse_music_search, url_encode, validate_youtube_url, MUSIC_CLIENT_VERSION, SONGS_FILTER};
use crate::{Item, ProgressFn, Provider, ProviderError, ReadyFn, Result};

const PLAYER: &str = "https://www.youtube.com/youtubei/v1/player?prettyPrint=false";
const SEARCH: &str = "https://www.youtube.com/youtubei/v1/search?prettyPrint=false";
const MUSIC_SEARCH: &str = "https://music.youtube.com/youtubei/v1/search?prettyPrint=false";
const BROWSE: &str = "https://www.youtube.com/youtubei/v1/browse?prettyPrint=false";
const OEMBED: &str = "https://www.youtube.com/oembed?format=json&url=";
const MAX_QUERY: usize = 200;
const MAX_SEARCH: u32 = 100;
/// How many ordinary results to fetch when the song search is thin. One page.
const GENERAL_COUNT: u32 = 20;
/// A player link stops working after some hours; keep it less than that.
const STREAM_FRESH: Duration = Duration::from_secs(2 * 60 * 60);
const CACHE_KEEP: usize = 8;
const CACHE_PARALLEL: usize = 2;
const CACHE_QUEUE: usize = 4;
/// Refuse tiny answers: an error page is not a song.
const MIN_AUDIO: u64 = 32 * 1024;

/// One Innertube client. These are the mobile-style ones that answer without cookies.
struct Client {
    name: &'static str,
    version: &'static str,
    /// `X-YouTube-Client-Name`.
    header: u32,
    user_agent: &'static str,
}

const CLIENTS: [Client; 3] = [
    Client {
        name: "VISIONOS",
        version: "1.02",
        header: 101,
        user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 15_7_3) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15",
    },
    Client {
        name: "ANDROID_VR",
        version: "1.65.10",
        header: 28,
        user_agent: "com.google.android.apps.youtube.vr.oculus/1.65.10 (Linux; U; Android 12L; eureka-user Build/SQ3A.220605.009.A1) gzip",
    },
    Client {
        name: "IOS",
        version: "21.26.4",
        header: 5,
        user_agent: "com.google.ios.youtube/21.26.4 (iPhone16,2; U; CPU iOS 18_3_2 like Mac OS X;)",
    },
];

fn client_context(c: &Client) -> Value {
    let mut client = json!({
        "clientName": c.name,
        "clientVersion": c.version,
        "hl": "en",
        "gl": "US",
        "userAgent": c.user_agent,
    });
    let extra = match c.name {
        "VISIONOS" => json!({
            "deviceMake": "Apple",
            "deviceModel": "RealityDevice17,1",
            "osName": "visionOS",
            "osVersion": "26.5.23O471",
        }),
        "ANDROID_VR" => json!({
            "deviceMake": "Oculus",
            "deviceModel": "Quest 3",
            "androidSdkVersion": 32,
            "osName": "Android",
            "osVersion": "12L",
        }),
        _ => json!({
            "deviceMake": "Apple",
            "deviceModel": "iPhone16,2",
            "osName": "iPhone",
            "osVersion": "18.3.2.22D82",
        }),
    };
    if let (Some(base), Some(extra)) = (client.as_object_mut(), extra.as_object()) {
        for (k, v) in extra {
            base.insert(k.clone(), v.clone());
        }
    }
    json!({ "client": client })
}

fn request_headers<'a>(c: &Client, header_name: &'a str, visitor: Option<&'a str>) -> Vec<(&'a str, &'a str)> {
    let mut headers = vec![
        ("X-YouTube-Client-Name", header_name),
        ("X-YouTube-Client-Version", c.version),
        ("Origin", "https://www.youtube.com"),
        ("User-Agent", c.user_agent),
        ("Accept-Language", "en-US,en;q=0.9"),
    ];
    if let Some(visitor) = visitor {
        headers.push(("X-Goog-Visitor-Id", visitor));
    }
    headers
}

/// A direct audio link and the details that came with it.
struct Resolved {
    stream: Stream,
    title: String,
    artist: String,
    duration_secs: Option<u32>,
    thumbnail: Option<String>,
}

struct Stream {
    url: String,
    size: Option<u64>,
    user_agent: String,
}

/// Resolved links, kept warm for prefetch and for a click right after a hover.
struct Cache {
    entries: Mutex<HashMap<String, (Instant, Arc<Resolved>)>>,
    /// Lookups ahead of time wait for one of these places.
    gate: Arc<tokio::sync::Semaphore>,
    speculative: Arc<AtomicUsize>,
    epoch: AtomicU64,
}

struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct Speculation {
    epoch: u64,
    _slot: Slot,
}

impl Cache {
    fn new() -> Cache {
        Cache {
            entries: Mutex::new(HashMap::new()),
            gate: Arc::new(tokio::sync::Semaphore::new(CACHE_PARALLEL)),
            speculative: Arc::new(AtomicUsize::new(0)),
            epoch: AtomicU64::new(0),
        }
    }

    fn get(&self, id: &str) -> Option<Arc<Resolved>> {
        self.entries.lock().unwrap().get(id).filter(|(at, _)| at.elapsed() < STREAM_FRESH).map(|(_, r)| r.clone())
    }

    fn put(&self, id: &str, resolved: Arc<Resolved>) {
        let mut map = self.entries.lock().unwrap();
        while map.len() >= CACHE_KEEP {
            let Some(old) = map.iter().min_by_key(|(_, (at, _))| *at).map(|(k, _)| k.clone()) else { break };
            map.remove(&old);
        }
        map.insert(id.to_string(), (Instant::now(), resolved));
    }

    fn forget(&self, id: &str) {
        self.entries.lock().unwrap().remove(id);
    }

    fn admit(&self) -> Option<Speculation> {
        self.speculative
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| (n < CACHE_PARALLEL + CACHE_QUEUE).then_some(n + 1))
            .ok()?;
        Some(Speculation { epoch: self.epoch.load(Ordering::SeqCst), _slot: Slot(self.speculative.clone()) })
    }

    fn supersede(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }

    fn wanted(&self, s: &Speculation) -> bool {
        s.epoch == self.epoch.load(Ordering::SeqCst)
    }
}

pub struct NativeYouTube {
    tagging: bool,
    visitor: tokio::sync::Mutex<Option<String>>,
    cache: Cache,
}

impl Default for NativeYouTube {
    fn default() -> Self {
        Self::new()
    }
}

impl NativeYouTube {
    pub fn new() -> NativeYouTube {
        NativeYouTube::with_tagging(false)
    }

    /// `tagging` adds title and artist after a download. The desktop flow only tags when
    /// ffmpeg exists, so the iOS entry point leaves this off.
    pub fn with_tagging(tagging: bool) -> NativeYouTube {
        NativeYouTube { tagging, visitor: tokio::sync::Mutex::new(None), cache: Cache::new() }
    }

    /// A visitor id, fetched once from a search answer. It is not a login; it only tells
    /// YouTube that this is a real client.
    async fn visitor_id(&self) -> Option<String> {
        if let Some(v) = self.visitor.lock().await.clone() {
            return Some(v);
        }
        let c = &CLIENTS[0];
        let header = c.header.to_string();
        let body = json!({ "context": client_context(c), "query": "music" }).to_string();
        let headers = request_headers(c, &header, None);
        let answer = http::post_json_value(SEARCH, body.as_bytes(), &headers, 2 * 1024 * 1024, "YouTube").await.ok()?;
        let id = answer.pointer("/responseContext/visitorData")?.as_str()?.to_string();
        *self.visitor.lock().await = Some(id.clone());
        Some(id)
    }

    async fn remember_visitor(&self, answer: &Value) {
        if let Some(id) = answer.pointer("/responseContext/visitorData").and_then(Value::as_str) {
            *self.visitor.lock().await = Some(id.to_string());
        }
    }

    async fn player(&self, id: &str, client: &Client, visitor: Option<&str>) -> Result<Value> {
        let header = client.header.to_string();
        let body = json!({
            "context": client_context(client),
            "videoId": id,
            "contentCheckOk": true,
            "racyCheckOk": true,
            "playbackContext": { "contentPlaybackContext": { "html5Preference": "HTML5_PREF_WANTS" } },
        })
        .to_string();
        let headers = request_headers(client, &header, visitor);
        http::post_json_value(PLAYER, body.as_bytes(), &headers, 8 * 1024 * 1024, "YouTube").await
    }

    /// Tries the clients in turn and returns the first playable AAC stream.
    async fn resolve(&self, id: &str) -> Result<Arc<Resolved>> {
        if !valid_video_id(id) {
            return Err(ProviderError::Invalid("that is not a YouTube video id".into()));
        }
        let visitor = self.visitor_id().await;
        let mut last = String::from("YouTube did not answer");
        let mut bot_check = false;
        for client in &CLIENTS {
            let answer = match self.player(id, client, visitor.as_deref()).await {
                Ok(answer) => answer,
                Err(e) => {
                    last = e.to_string();
                    continue;
                }
            };
            self.remember_visitor(&answer).await;
            let status = answer.pointer("/playabilityStatus/status").and_then(Value::as_str).unwrap_or("");
            if status == "OK" {
                let formats = answer.pointer("/streamingData/adaptiveFormats").and_then(Value::as_array);
                if let Some((url, size)) = formats.and_then(|f| pick_audio(f)) {
                    let details = answer.get("videoDetails");
                    let title = details.and_then(|d| d.get("title")).and_then(Value::as_str).unwrap_or(id).to_string();
                    let artist = details.and_then(|d| d.get("author")).and_then(Value::as_str).unwrap_or("").to_string();
                    let duration_secs = details
                        .and_then(|d| d.get("lengthSeconds"))
                        .and_then(|v| v.as_str().and_then(|s| s.parse().ok()).or_else(|| v.as_u64()))
                        .map(|d| d as u32);
                    let thumbnail = details
                        .and_then(|d| d.pointer("/thumbnail/thumbnails"))
                        .and_then(Value::as_array)
                        .and_then(|t| t.last())
                        .and_then(|t| t.get("url"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or_else(|| Some(meowtify_core::youtube_thumbnail(id)));
                    return Ok(Arc::new(Resolved {
                        stream: Stream { url, size, user_agent: client.user_agent.to_string() },
                        title,
                        artist,
                        duration_secs,
                        thumbnail,
                    }));
                }
                last = "YouTube sent no AAC audio for this song".into();
                continue;
            }
            let reason = answer
                .pointer("/playabilityStatus/reason")
                .and_then(Value::as_str)
                .filter(|r| !r.is_empty())
                .unwrap_or("the video cannot be played");
            last = reason.to_string();
            if reason.to_lowercase().contains("bot") {
                bot_check = true;
            }
        }
        if bot_check {
            Err(ProviderError::Tool(
                "YouTube wants a sign-in check for this song. Try another source, or try again later.".into(),
            ))
        } else {
            Err(ProviderError::Tool(format!("YouTube: {last}")))
        }
    }

    async fn resolved(&self, id: &str) -> Result<Arc<Resolved>> {
        if let Some(found) = self.cache.get(id) {
            return Ok(found);
        }
        let found = self.resolve(id).await?;
        self.cache.put(id, found.clone());
        Ok(found)
    }

    /// The YouTube Music song search. It needs no visitor id and carries artist and length.
    async fn music_search(&self, query: &str, limit: usize) -> Result<Vec<Item>> {
        let body = json!({
            "context": { "client": { "clientName": "WEB_REMIX", "clientVersion": MUSIC_CLIENT_VERSION, "hl": "en", "gl": "US" } },
            "query": query,
            "params": SONGS_FILTER,
        })
        .to_string();
        let headers = [("User-Agent", http::DESKTOP_UA), ("Origin", "https://music.youtube.com")];
        let mut reply = http::post_json(MUSIC_SEARCH, body.as_bytes(), &headers).await?;
        let status = reply.status;
        let bytes = reply.bytes(8 * 1024 * 1024).await?;
        if status != 200 {
            return Err(http::status_error("YouTube Music", status, &bytes));
        }
        parse_music_search(&bytes, limit)
    }

    /// The ordinary search, used to fill in when the song search has too few rows.
    async fn general_search(&self, query: &str, limit: usize) -> Result<Vec<Item>> {
        let c = &CLIENTS[0];
        let header = c.header.to_string();
        let body = json!({ "context": client_context(c), "query": query }).to_string();
        let headers = request_headers(c, &header, None);
        let mut reply = http::post_json(SEARCH, body.as_bytes(), &headers).await?;
        let status = reply.status;
        let bytes = reply.bytes(8 * 1024 * 1024).await?;
        if status != 200 {
            return Err(http::status_error("YouTube", status, &bytes));
        }
        let answer: Value = serde_json::from_slice(&bytes).map_err(|e| ProviderError::Parse(e.to_string()))?;
        Ok(parse_general_search(&answer, limit))
    }

    async fn oembed(&self, id: &str) -> Option<(String, String, Option<String>)> {
        let url = format!("{OEMBED}{}", url_encode(&format!("https://www.youtube.com/watch?v={id}")));
        let answer = http::get_json(&url, &[("User-Agent", http::DESKTOP_UA)], 256 * 1024, "YouTube").await.ok()?;
        let title = answer.get("title").and_then(Value::as_str)?.to_string();
        let artist = answer.get("author_name").and_then(Value::as_str).unwrap_or("").to_string();
        let thumbnail = answer.get("thumbnail_url").and_then(Value::as_str).map(str::to_string);
        Some((title, artist, thumbnail))
    }

    /// A playlist link, read through the browse endpoint.
    async fn resolve_playlist(&self, list: &str) -> Result<Vec<Item>> {
        if !valid_playlist_id(list) {
            return Err(ProviderError::Invalid("that is not a YouTube playlist id".into()));
        }
        let c = &CLIENTS[0];
        let header = c.header.to_string();
        let body = json!({ "context": client_context(c), "browseId": format!("VL{list}") }).to_string();
        let visitor = self.visitor.lock().await.clone();
        let headers = request_headers(c, &header, visitor.as_deref());
        let answer = http::post_json_value(BROWSE, body.as_bytes(), &headers, 8 * 1024 * 1024, "YouTube").await?;
        let items = parse_playlist(&answer, 200);
        if items.is_empty() {
            return Err(ProviderError::Tool("YouTube did not list that playlist".into()));
        }
        Ok(items)
    }

    async fn fetch_stream(&self, resolved: &Resolved, path: &Path, progress: ProgressFn, ready: Option<&ReadyFn>) -> Result<u64> {
        use meowtify_core::growth::{self, Growth};
        let growth = Growth::new();
        growth::register(path, growth.clone());
        let mut guard = Growing { path: path.to_path_buf(), growth: growth.clone(), armed: true };
        let mut file = tokio::fs::File::create(path).await.map_err(|e| ProviderError::Io(e.to_string()))?;
        let headers = [("User-Agent", resolved.stream.user_agent.as_str()), ("Accept", "*/*")];
        let mut offset = 0u64;
        let mut total = resolved.stream.size.filter(|t| *t > 0);
        let mut announced = false;
        let mut buf = vec![0u8; 64 * 1024];

        loop {
            if total.is_some_and(|t| offset >= t) {
                break;
            }
            let want = fetch::chunk_len(offset, total);
            let mut reply = http::get_range(&resolved.stream.url, offset, want, &headers).await?;
            let whole = match reply.status {
                206 => false,
                200 if offset == 0 => true,
                416 if offset > 0 => break,
                status => return Err(ProviderError::Tool(format!("YouTube's audio server answered {status}"))),
            };
            if let Some(range) = reply.header("content-range").and_then(fetch::parse_content_range) {
                if range.0 != offset {
                    return Err(ProviderError::Tool(format!("the server sent bytes from {}, not {offset}", range.0)));
                }
                if let Some(t) = range.2 {
                    total = Some(t);
                }
            }
            let mut got = 0u64;
            loop {
                let n = match tokio::time::timeout(Duration::from_secs(120), reply.read(&mut buf)).await {
                    Ok(r) => r?,
                    Err(_) => return Err(ProviderError::Tool("the download timed out".into())),
                };
                if n == 0 {
                    break;
                }
                file.write_all(&buf[..n]).await.map_err(|e| ProviderError::Io(e.to_string()))?;                got += n as u64;
                offset += n as u64;
                growth.set_written(offset);
                if let Some(t) = total {
                    progress((offset as f64 / t as f64 * 100.0).min(99.0) as f32);
                }
                if !announced && offset >= READY_BYTES {
                    announced = true;
                    if let Some(ready) = ready {
                        ready(path);
                    }
                }
                if !whole && got >= want {
                    break;
                }
            }
            file.flush().await.map_err(|e| ProviderError::Io(e.to_string()))?;
            if got == 0 || whole || (total.is_none() && got < want) {
                break;
            }
        }
        drop(file);
        if let Some(t) = total {
            if offset != t {
                return Err(ProviderError::Tool(format!("the download has {offset} bytes, the server said {t}")));
            }
        }
        if offset < MIN_AUDIO {
            return Err(ProviderError::Tool("YouTube sent no audio for this song".into()));
        }
        growth.finish(offset);
        guard.armed = false;
        growth::forget(path);
        if !announced {
            if let Some(ready) = ready {
                ready(path);
            }
        }
        progress(100.0);
        Ok(offset)
    }

    fn media_path(item: &Item, dest_dir: &Path) -> PathBuf {
        dest_dir.join(crate::stream::file_name(item))
    }
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

/// The best decodable audio format: MP4/AAC first, then MP3. Opus in WebM is left out
/// because the player cannot decode it.
fn pick_audio(formats: &[Value]) -> Option<(String, Option<u64>)> {
    let mut best: Option<(i64, &Value)> = None;
    for format in formats {
        let Some(url) = format.get("url").and_then(Value::as_str).filter(|u| u.starts_with("https://")) else { continue };
        if url.chars().any(|c| c.is_control()) {
            continue;
        }
        let mime = format.get("mimeType").and_then(Value::as_str).unwrap_or("");
        let itag = format.get("itag").and_then(Value::as_i64).unwrap_or(0);
        let base = if mime.starts_with("audio/mp4") {
            100
        } else if mime.starts_with("audio/mpeg") {
            90
        } else {
            continue;
        };
        let score = base + if itag == 140 { 10 } else { 0 } + format.get("bitrate").and_then(Value::as_i64).unwrap_or(0) / 1000;
        if best.is_none_or(|(s, _)| score > s) {
            best = Some((score, format));
        }
    }
    best.map(|(_, format)| {
        let url = format.get("url").and_then(Value::as_str).unwrap_or_default().to_string();
        let size = format.get("contentLength").and_then(|v| {
            v.as_str().and_then(|s| s.parse::<u64>().ok()).or_else(|| v.as_u64())
        });
        (url, size)
    })
}

fn text_of(value: Option<&Value>) -> String {
    let Some(value) = value else { return String::new() };
    if let Some(text) = value.get("simpleText").and_then(Value::as_str) {
        return text.to_string();
    }
    value
        .get("runs")
        .and_then(Value::as_array)
        .map(|runs| runs.iter().filter_map(|r| r.get("text")?.as_str()).collect::<String>())
        .unwrap_or_default()
}

/// Reads ordinary search rows (`videoRenderer` or `compactVideoRenderer`).
fn parse_general_search(answer: &Value, limit: usize) -> Vec<Item> {
    let sections = answer
        .pointer("/contents/twoColumnSearchResultsRenderer/primaryContents/sectionListRenderer/contents")
        .or_else(|| answer.pointer("/contents/sectionListRenderer/contents"))
        .and_then(Value::as_array);
    let mut items = Vec::with_capacity(limit);
    'outer: for section in sections.into_iter().flatten() {
        let Some(rows) = section.pointer("/itemSectionRenderer/contents").and_then(Value::as_array) else { continue };
        for row in rows {
            let Some(r) = row.get("videoRenderer").or_else(|| row.get("compactVideoRenderer")) else { continue };
            let Some(id) = r.get("videoId").and_then(Value::as_str).filter(|i| valid_video_id(i)) else { continue };
            let title = text_of(r.get("title"));
            if title.is_empty() {
                continue;
            }
            let artist = text_of(r.get("longBylineText").or_else(|| r.get("shortBylineText")));
            let duration_secs = r.get("lengthText").map(|t| text_of(Some(t))).and_then(|t| parse_clock(&t));
            let thumbnail = r
                .pointer("/thumbnail/thumbnails")
                .and_then(Value::as_array)
                .and_then(|t| t.last())
                .and_then(|t| t.get("url"))
                .and_then(Value::as_str)
                .map(str::to_string);
            items.push(Item {
                provider: "youtube",
                id: id.to_string(),
                title,
                artist,
                duration_secs,
                url: format!("https://www.youtube.com/watch?v={id}"),
                thumbnail,
            });
            if items.len() >= limit {
                break 'outer;
            }
        }
    }
    items
}

/// Reads playlist rows. Continuations are ignored; the first page is enough for a paste.
fn parse_playlist(answer: &Value, limit: usize) -> Vec<Item> {
    let mut rows: Vec<&Value> = Vec::new();
    fn walk<'a>(value: &'a Value, out: &mut Vec<&'a Value>) {
        match value {
            Value::Object(map) => {
                if let Some(row) = map.get("playlistVideoRenderer") {
                    out.push(row);
                }
                for child in map.values() {
                    walk(child, out);
                }
            }
            Value::Array(list) => {
                for child in list {
                    walk(child, out);
                }
            }
            _ => {}
        }
    }
    walk(answer, &mut rows);
    let mut items = Vec::with_capacity(limit);
    for r in rows {
        let Some(id) = r.get("videoId").and_then(Value::as_str).filter(|i| valid_video_id(i)) else { continue };
        let title = text_of(r.get("title"));
        if title.is_empty() {
            continue;
        }
        let artist = text_of(r.get("shortBylineText").or_else(|| r.get("longBylineText")));
        let duration_secs = r.get("lengthText").map(|t| text_of(Some(t))).and_then(|t| parse_clock(&t));
        items.push(Item {
            provider: "youtube",
            id: id.to_string(),
            title,
            artist,
            duration_secs,
            url: format!("https://www.youtube.com/watch?v={id}"),
            thumbnail: Some(meowtify_core::youtube_thumbnail(id)),
        });
        if items.len() >= limit {
            break;
        }
    }
    items
}

fn valid_video_id(id: &str) -> bool {
    id.len() == 11 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn valid_playlist_id(id: &str) -> bool {
    (10..=80).contains(&id.len()) && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn query_param(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1.split('#').next()?;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=')?;
        if k == key && !v.is_empty() {
            return Some(v.to_string());
        }
    }
    None
}

/// The 11 character video id in a YouTube link, from the shapes the app accepts.
fn video_id(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let host = authority.split(':').next().unwrap_or("").to_ascii_lowercase();
    if !["youtube.com", "www.youtube.com", "m.youtube.com", "music.youtube.com", "youtu.be"].contains(&host.as_str()) {
        return None;
    }
    if host == "youtu.be" {
        let id = path.trim_start_matches('/').split(['/', '?', '#']).next().unwrap_or("");
        return valid_video_id(id).then(|| id.to_string());
    }
    if let Some(id) = query_param(url, "v") {
        if valid_video_id(&id) {
            return Some(id);
        }
    }
    for prefix in ["/shorts/", "/embed/", "/live/", "/v/"] {
        if let Some(tail) = path.strip_prefix(prefix) {
            let id = tail.split(['/', '?', '#']).next().unwrap_or("");
            if valid_video_id(id) {
                return Some(id.to_string());
            }
        }
    }
    None
}

#[async_trait]
impl Provider for NativeYouTube {
    async fn search(&self, query: &str, limit: u32) -> Result<Vec<Item>> {
        let q = query.trim();
        if q.is_empty() || q.chars().count() > MAX_QUERY || q.chars().any(|c| c.is_control()) {
            return Err(ProviderError::Invalid("enter a search of 1 to 200 characters".into()));
        }
        let n = limit.clamp(1, MAX_SEARCH) as usize;

        let mut songs = Vec::new();
        if let Ok(items) = self.music_search(q, n).await {
            if !items.is_empty() && (items.len() >= n || n <= 20) {
                return Ok(items);
            }
            songs = items;
        }
        let general = self.general_search(q, GENERAL_COUNT.max(n as u32) as usize).await.unwrap_or_default();
        if songs.is_empty() && general.is_empty() {
            return Err(ProviderError::Tool("YouTube found nothing; try another search".into()));
        }
        Ok(merge(songs, general, n))
    }

    async fn resolve(&self, url: &str) -> Result<Vec<Item>> {
        let url = validate_youtube_url(url)?;
        if let Some(id) = video_id(&url) {
            match self.resolved(&id).await {
                Ok(found) => Ok(vec![Item {
                    provider: "youtube",
                    id: id.clone(),
                    title: found.title.clone(),
                    artist: found.artist.clone(),
                    duration_secs: found.duration_secs,
                    url: format!("https://www.youtube.com/watch?v={id}"),
                    thumbnail: found.thumbnail.clone(),
                }]),
                Err(e) => match self.oembed(&id).await {
                    Some((title, artist, thumbnail)) => Ok(vec![Item {
                        provider: "youtube",
                        id: id.clone(),
                        title,
                        artist,
                        duration_secs: None,
                        url: format!("https://www.youtube.com/watch?v={id}"),
                        thumbnail,
                    }]),
                    None => Err(e),
                },
            }
        } else if let Some(list) = query_param(&url, "list") {
            self.resolve_playlist(&list).await
        } else {
            Err(ProviderError::Invalid("that link does not name a video or a playlist".into()))
        }
    }

    async fn prefetch(&self, item: &Item) {
        if self.cache.get(&item.id).is_some() {
            return;
        }
        let Some(speculation) = self.cache.admit() else { return };
        let Ok(_turn) = self.cache.gate.clone().acquire_owned().await else { return };
        if !self.cache.wanted(&speculation) || self.cache.get(&item.id).is_some() {
            return;
        }
        if let Ok(found) = self.resolve(&item.id).await {
            self.cache.put(&item.id, found);
        }
    }

    fn cancel_prefetch(&self) {
        self.cache.supersede();
    }

    async fn stream(&self, item: &Item, dest_dir: &Path, progress: ProgressFn, ready: ReadyFn) -> Result<PathBuf> {
        std::fs::create_dir_all(dest_dir).map_err(|e| ProviderError::Io(e.to_string()))?;
        let found = self.resolved(&item.id).await?;
        let path = Self::media_path(item, dest_dir);
        match self.fetch_stream(&found, &path, progress, Some(&ready)).await {
            Ok(_) => Ok(path),
            Err(e) => {
                self.cache.forget(&item.id);
                Err(e)
            }
        }
    }

    async fn finish(&self, item: &Item, path: &Path) -> Result<()> {
        if !self.tagging {
            return Ok(());
        }
        let path = path.to_path_buf();
        let item = item.clone();
        tokio::task::spawn_blocking(move || crate::native::tags::tag(&path, &item))
            .await
            .map_err(|e| ProviderError::Io(e.to_string()))?
    }

    async fn download(&self, item: &Item, dest_dir: &Path, progress: ProgressFn) -> Result<PathBuf> {
        std::fs::create_dir_all(dest_dir).map_err(|e| ProviderError::Io(e.to_string()))?;
        let found = self.resolved(&item.id).await?;
        let path = Self::media_path(item, dest_dir);
        let part = path.with_extension("m4a.part");
        let guard = fetch::PartFile::new(part.clone());
        let headers = [("User-Agent", found.stream.user_agent.as_str()), ("Accept", "*/*")];
        let result = fetch::download_ranged(&found.stream.url, &headers, &part, found.stream.size, progress.clone()).await;
        let size = match result {
            Ok(size) => size,
            Err(e) => {
                self.cache.forget(&item.id);
                return Err(e);
            }
        };
        if size < MIN_AUDIO {
            return Err(ProviderError::Tool("YouTube sent no audio for this song".into()));
        }
        std::fs::rename(&part, &path).map_err(|e| ProviderError::Io(e.to_string()))?;
        guard.keep();
        progress(100.0);
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_aac_and_refuses_webm() {
        let formats = json!([
            { "itag": 251, "mimeType": "audio/webm; codecs=\"opus\"", "url": "https://x/a.webm", "bitrate": 160000 },
            { "itag": 140, "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"", "url": "https://x/a.m4a", "contentLength": "4250167", "bitrate": 129000 },
            { "itag": 139, "mimeType": "audio/mp4; codecs=\"mp4a.40.5\"", "url": "https://x/b.m4a", "bitrate": 48000 }
        ]);
        let (url, size) = pick_audio(formats.as_array().unwrap()).unwrap();
        assert_eq!(url, "https://x/a.m4a");
        assert_eq!(size, Some(4_250_167));
    }

    #[test]
    fn only_https_formats_are_used() {
        let formats = json!([
            { "itag": 140, "mimeType": "audio/mp4", "url": "http://x/a.m4a" },
            { "itag": 140, "mimeType": "audio/mp4", "url": "https://x/a.m4a" }
        ]);
        assert_eq!(pick_audio(formats.as_array().unwrap()).unwrap().0, "https://x/a.m4a");
        assert!(pick_audio(&[]).is_none());
    }

    #[test]
    fn finds_video_ids_in_the_usual_links() {
        assert_eq!(video_id("https://www.youtube.com/watch?v=dQw4w9WgXcQ").as_deref(), Some("dQw4w9WgXcQ"));
        assert_eq!(video_id("https://youtu.be/dQw4w9WgXcQ?t=3").as_deref(), Some("dQw4w9WgXcQ"));
        assert_eq!(video_id("https://m.youtube.com/shorts/dQw4w9WgXcQ").as_deref(), Some("dQw4w9WgXcQ"));
        assert_eq!(video_id("https://www.youtube.com/embed/dQw4w9WgXcQ").as_deref(), Some("dQw4w9WgXcQ"));
        assert_eq!(video_id("https://music.youtube.com/watch?v=dQw4w9WgXcQ&list=RDAMVM"), Some("dQw4w9WgXcQ".into()));
        assert!(video_id("https://www.youtube.com/watch?v=short").is_none());
        assert!(video_id("https://evil.example/watch?v=dQw4w9WgXcQ").is_none());
    }

    #[test]
    fn reads_ordinary_search_rows() {
        let answer = json!({ "contents": { "twoColumnSearchResultsRenderer": { "primaryContents": { "sectionListRenderer": { "contents": [
            { "itemSectionRenderer": { "contents": [
                { "videoRenderer": {
                    "videoId": "dQw4w9WgXcQ",
                    "title": { "runs": [ { "text": "Never Gonna Give You Up" } ] },
                    "longBylineText": { "runs": [ { "text": "Rick Astley" } ] },
                    "lengthText": { "simpleText": "3:33" },
                    "thumbnail": { "thumbnails": [ { "url": "https://i.ytimg.com/vi/dQw4w9WgXcQ/hq.jpg" } ] }
                } },
                { "compactVideoRenderer": { "videoId": "aaaaaaaaaaa", "title": { "simpleText": "Other" } } },
                { "playlistRenderer": { "title": "not a video" } }
            ] } }
        ] } } } } });
        let items = parse_general_search(&answer, 10);
        assert_eq!(items.len(), 2);
        assert_eq!((items[0].id.as_str(), items[0].artist.as_str(), items[0].duration_secs), ("dQw4w9WgXcQ", "Rick Astley", Some(213)));
        assert_eq!(items[1].id, "aaaaaaaaaaa");
    }

    #[test]
    fn reads_playlist_rows() {
        let answer = json!({ "contents": { "twoColumnBrowseResultsRenderer": { "tabs": [ { "tabRenderer": { "content": {
            "sectionListRenderer": { "contents": [ { "itemSectionRenderer": { "contents": [ { "playlistVideoListRenderer": { "contents": [
                { "playlistVideoRenderer": { "videoId": "dQw4w9WgXcQ", "title": { "simpleText": "One" }, "lengthText": { "simpleText": "1:00" } } },
                { "playlistVideoRenderer": { "videoId": "bad", "title": { "simpleText": "Bad" } } }
            ] } } ] } } ] }
        } } } ] } } });
        let items = parse_playlist(&answer, 10);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].duration_secs, Some(60));
    }

    #[test]
    fn context_names_every_client() {
        for client in &CLIENTS {
            let ctx = client_context(client);
            assert_eq!(ctx.pointer("/client/clientName").and_then(Value::as_str), Some(client.name));
            assert_eq!(ctx.pointer("/client/clientVersion").and_then(Value::as_str), Some(client.version));
        }
    }

    #[test]
    fn query_values_are_read() {
        assert_eq!(query_param("https://x/y?a=1&list=PL123", "list").as_deref(), Some("PL123"));
        assert_eq!(query_param("https://x/y?a=1#list=no", "list"), None);
        assert_eq!(query_param("https://x/y", "list"), None);
    }

    #[test]
    fn ids_are_checked() {
        assert!(valid_video_id("dQw4w9WgXcQ"));
        assert!(!valid_video_id("dQw4w9WgXc"));
        assert!(!valid_video_id("dQw4w9WgXc!"));
        assert!(valid_playlist_id("PL1234567890"));
        assert!(!valid_playlist_id("short"));
    }

    /// Needs the network: `cargo test -p meowtify-provider --features native-extract live_native -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_native_search_and_download() {
        let yt = NativeYouTube::new();
        let items = yt.search("tsubi club", 5).await.unwrap();
        eprintln!("{} results, first {:?}", items.len(), items.first());
        assert!(!items.is_empty());
        let dir = std::env::temp_dir().join(format!("meowtify-native-{}", std::process::id()));
        let path = yt.download(&items[0], &dir, Arc::new(|p| eprintln!("{p:.0}%"))).await.unwrap();
        eprintln!("saved {} ({} bytes)", path.display(), std::fs::metadata(&path).unwrap().len());
        assert!(std::fs::metadata(&path).unwrap().len() > MIN_AUDIO);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Needs the network: `cargo test -p meowtify-provider --features native-extract live_native_general -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_native_general_search_is_a_fallback() {
        let yt = NativeYouTube::new();
        let items = yt.general_search("funny cat compilation 2019", 5).await.unwrap();
        eprintln!("{} ordinary results, first {:?}", items.len(), items.first());
        assert!(!items.is_empty());
    }

    /// Needs the network: `cargo test -p meowtify-provider --features native-extract live_native_stream -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_native_stream_reports_ready_before_the_end() {
        let yt = NativeYouTube::new();
        let item = Item {
            provider: "youtube",
            id: "P2oz2RBtjWQ".into(),
            title: "stream test".into(),
            artist: String::new(),
            duration_secs: None,
            url: "https://www.youtube.com/watch?v=P2oz2RBtjWQ".into(),
            thumbnail: None,
        };
        let dir = std::env::temp_dir().join(format!("meowtify-native-stream-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let announced = Arc::new(Mutex::new(None));
        let ready: ReadyFn = {
            let announced = announced.clone();
            Arc::new(move |path: &Path| {
                let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                *announced.lock().unwrap() = Some((size, meowtify_core::growth::lookup(path).is_some()));
            })
        };
        let path = yt.stream(&item, &dir, Arc::new(|_| {}), ready).await.unwrap();
        let size = std::fs::metadata(&path).unwrap().len();
        eprintln!("ready {:?}, final {size} bytes: {}", announced.lock().unwrap(), path.display());
        assert!(announced.lock().unwrap().is_some(), "ready was never called");
        assert!(size > MIN_AUDIO);
        assert!(meowtify_core::growth::lookup(&path).is_none(), "the growth record was not cleared");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
