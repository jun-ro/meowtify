//! YouTube through the `yt-dlp` executable.
//!
//! yt-dlp is run as a subprocess, so none of its memory counts against the
//! app. Every argument that comes from the user is checked first, and links go
//! after `--` so they can never be read as options.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::ytdlp::YtDlp;
use crate::{Item, ProgressFn, Provider, ProviderError, ReadyFn, Result};

const PROGRESS_PREFIX: &str = "MEOWPROGRESS ";
const FILE_PREFIX: &str = "MEOWFILE:";
const MAX_QUERY: usize = 200;
/// The most results one search may ask for. The list grows in steps as the user scrolls.
const MAX_SEARCH: u32 = 100;
/// One page of YouTube Music songs holds about this many rows; asking for more needs the slow path.
const FAST_PAGE: u32 = 20;
/// How many ordinary results to fetch. One page, so no extra request.
const GENERAL_COUNT: u32 = 20;
/// YouTube Music search filter: songs only.
const SONGS_FILTER: &str = "EgWKAQIIAWoKEAkQBRAKEAMQBA%3D%3D";
const HOSTS: [&str; 5] = ["youtube.com", "www.youtube.com", "m.youtube.com", "music.youtube.com", "youtu.be"];

/// Paths of the programs this provider runs.
#[derive(Debug, Clone)]
pub struct Tools {
    pub yt_dlp: PathBuf,
    /// Without ffmpeg, downloads keep the source format and carry no tags.
    pub ffmpeg: Option<PathBuf>,
}

impl Tools {
    /// Looks in `tools_dir` first, then in `PATH`. Fails only when yt-dlp is missing.
    pub fn locate(tools_dir: &Path) -> Result<Tools> {
        let yt_dlp = find_program("yt-dlp", tools_dir).ok_or(ProviderError::ToolMissing("yt-dlp"))?;
        Ok(Tools { yt_dlp, ffmpeg: find_program("ffmpeg", tools_dir) })
    }
}

impl Tools {
    /// Like `locate`, but never fails. A missing yt-dlp gets the tools-folder path,
    /// where an update installs it.
    pub fn locate_or_default(tools_dir: &Path) -> Tools {
        let yt_dlp = find_program("yt-dlp", tools_dir)
            .unwrap_or_else(|| tools_dir.join(if cfg!(windows) { "yt-dlp.exe" } else { "yt-dlp" }));
        Tools { yt_dlp, ffmpeg: find_program("ffmpeg", tools_dir) }
    }
}

fn find_program(name: &str, tools_dir: &Path) -> Option<PathBuf> {
    let file = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    // The folder build of yt-dlp starts faster than the single file, so it wins.
    if cfg!(windows) && name == "yt-dlp" {
        let dir_build = tools_dir.join(crate::ytdlp::DIR_NAME).join(&file);
        if dir_build.is_file() {
            return Some(dir_build);
        }
    }
    let local = tools_dir.join(&file);
    if local.is_file() {
        return Some(local);
    }
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(&file)).find(|p| p.is_file())
}

pub(crate) fn plain_command(program: &Path) -> Command {
    let mut c = Command::new(program);
    c.env("PYTHONUTF8", "1").env("PYTHONIOENCODING", "utf-8");
    c.stdin(Stdio::null()).kill_on_drop(true);
    #[cfg(windows)]
    c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    c
}

/// `yt-dlp --version`, or `None` when it cannot be read within 10 seconds.
pub async fn yt_dlp_version(program: &Path) -> Option<String> {
    let mut c = plain_command(program);
    c.arg("--version");
    match tokio::time::timeout(std::time::Duration::from_secs(10), c.output()).await {
        Ok(Ok(o)) if o.status.success() => Some(String::from_utf8_lossy(&o.stdout).trim().to_string()),
        _ => None,
    }
}

/// Looked-up yt-dlp info files (`--write-info-json`), by video id. Loading one with
/// `--load-info-json` skips the YouTube lookup, which is most of the time a download
/// takes to start (about 1.5 s of 2.4 s).
struct InfoCache {
    dir: PathBuf,
    entries: Mutex<HashMap<String, (Instant, Arc<tokio::sync::OnceCell<Option<PathBuf>>>)>>,
    /// Limits lookups started ahead of time, so hovering does not start a crowd of yt-dlp.
    gate: Arc<tokio::sync::Semaphore>,
    /// Lookups ahead of time that wait for a turn or run. Bounded, so hovering over a long
    /// list cannot queue a task per row.
    speculative: Arc<AtomicUsize>,
    /// Counts result lists. A lookup ahead of time that waited through a new search is dropped:
    /// nobody can click its row any more.
    epoch: AtomicU64,
}

/// Releases a place in the queue of lookups ahead of time.
struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Permission to wait for a lookup ahead of time.
struct Speculation {
    epoch: u64,
    _slot: Slot,
}

/// Stream links in an info file stop working after some hours.
const INFO_FRESH: Duration = Duration::from_secs(3 * 60 * 60);
const INFO_KEEP: usize = 8;
const INFO_PARALLEL: usize = 2;
/// Lookups ahead of time that may wait behind the running ones. Others are dropped: a hover
/// that mattered will be repeated, and a click does its own lookup.
const INFO_QUEUE: usize = 4;

impl InfoCache {
    fn new() -> InfoCache {
        // One folder per instance: another instance (or an earlier run) never shares files
        // with this one, so deleting here cannot remove a file that is in use elsewhere.
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join("meowtify-info");
        let dir = root.join(format!("{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        // Remove folders of earlier runs. Info files go stale after INFO_FRESH, so a folder
        // untouched for much longer belongs to no running instance.
        if let Ok(rd) = std::fs::read_dir(&root) {
            for e in rd.flatten() {
                let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_none_or(|age| age > INFO_FRESH * 2);
                if old {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
        InfoCache {
            dir,
            entries: Mutex::new(HashMap::new()),
            gate: Arc::new(tokio::sync::Semaphore::new(INFO_PARALLEL)),
            speculative: Arc::new(AtomicUsize::new(0)),
            epoch: AtomicU64::new(0),
        }
    }

    /// A place to wait for a lookup ahead of time, or `None` when the queue is full.
    fn admit(&self) -> Option<Speculation> {
        self.speculative.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| (n < INFO_PARALLEL + INFO_QUEUE).then_some(n + 1)).ok()?;
        Some(Speculation { epoch: self.epoch.load(Ordering::SeqCst), _slot: Slot(self.speculative.clone()) })
    }

    /// The results changed: lookups ahead of time that are still waiting are not wanted.
    fn supersede(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }

    fn wanted(&self, s: &Speculation) -> bool {
        s.epoch == self.epoch.load(Ordering::SeqCst)
    }

    /// Whether a fresh lookup for `id` exists or is running.
    fn has(&self, id: &str) -> bool {
        self.entries.lock().unwrap().get(id).is_some_and(|(at, _)| at.elapsed() < INFO_FRESH)
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.info.json"))
    }

    /// The shared cell for `id`: a fresh one when there is none or it is stale.
    fn cell(&self, id: &str) -> Arc<tokio::sync::OnceCell<Option<PathBuf>>> {
        let mut map = self.entries.lock().unwrap();
        if let Some((at, cell)) = map.get(id) {
            if at.elapsed() < INFO_FRESH {
                return cell.clone();
            }
        }
        // Drop the oldest ones nobody uses right now, until there is room. A cell that is shared
        // with a running lookup or download (more than the map holds) keeps its file; when all
        // are in use the map is over its size only for as long as they run, and it comes back
        // down the next time a cell is made.
        while map.len() >= INFO_KEEP {
            let idle = map.iter().filter(|(_, (_, c))| Arc::strong_count(c) == 1).min_by_key(|(_, (at, _))| *at).map(|(k, _)| k.clone());
            let Some(old) = idle else { break };
            map.remove(&old);
            let _ = std::fs::remove_file(self.path(&old));
        }
        let cell = Arc::new(tokio::sync::OnceCell::new());
        map.insert(id.to_string(), (Instant::now(), cell.clone()));
        cell
    }

    fn forget(&self, id: &str) {
        self.entries.lock().unwrap().remove(id);
        let _ = std::fs::remove_file(self.path(id));
    }
}

pub struct YouTube {
    tools: Tools,
    yt_dlp: Arc<YtDlp>,
    info: InfoCache,
}

impl YouTube {
    pub fn new(tools: Tools) -> YouTube {
        let dir = tools.yt_dlp.parent().map(Path::to_path_buf).unwrap_or_default();
        let yt_dlp = Arc::new(YtDlp::new(tools.yt_dlp.clone(), dir));
        YouTube { tools, yt_dlp, info: InfoCache::new() }
    }

    /// Shares `yt_dlp`, so an update made from Settings applies to the next download.
    pub fn with_yt_dlp(tools: Tools, yt_dlp: Arc<YtDlp>) -> YouTube {
        YouTube { tools, yt_dlp, info: InfoCache::new() }
    }

    fn format_selector(&self) -> &'static str {
        // m4a (AAC) is what the player decodes.
        if self.tools.ffmpeg.is_some() {
            "bestaudio[ext=m4a]/bestaudio"
        } else {
            "bestaudio[ext=m4a]"
        }
    }

    /// The info file for a video: waits for a lookup that is already running, or does
    /// it now. `None` when the lookup failed (the download then does its own).
    async fn info_for(&self, id: &str, url: &str) -> Option<PathBuf> {
        let cell = self.info.cell(id);
        let found = cell.get_or_init(|| self.lookup(id, url)).await.clone();
        if found.is_none() {
            self.info.forget(id); // let a later try start fresh
        }
        found
    }

    /// Looks `id` up before anyone asked, when a turn is free. Waits at most for the few places
    /// in the queue, takes no cache entry while waiting, and gives up when the results change.
    async fn info_ahead(&self, id: &str, url: &str) {
        let Some(speculation) = self.info.admit() else { return };
        if self.info.has(id) {
            return;
        }
        let Ok(_turn) = self.info.gate.clone().acquire_owned().await else { return };
        if !self.info.wanted(&speculation) || self.info.has(id) {
            return;
        }
        let _ = self.info_for(id, url).await;
    }

    async fn lookup(&self, id: &str, url: &str) -> Option<PathBuf> {
        std::fs::create_dir_all(&self.info.dir).ok()?;
        let mut cmd = self.command();
        cmd.args(["--no-playlist", "--skip-download", "--write-info-json", "-f", self.format_selector(), "-o"]);
        cmd.arg(self.info.dir.join(id)).arg("--").arg(url);
        let out = tokio::time::timeout(Duration::from_secs(40), cmd.output()).await.ok()?.ok()?;
        let path = self.info.path(id);
        (out.status.success() && path.is_file()).then_some(path)
    }

    pub(crate) fn command(&self) -> Command {
        let mut c = Command::new(self.yt_dlp.path());
        // Ignore the user's own yt-dlp config so output stays predictable.
        c.args(["--ignore-config", "--no-warnings", "--no-colors"]);
        crate::net::apply(&mut c);
        // Without this, Windows prints non-ASCII titles in the legacy code page
        // and the reported file path no longer matches the real file.
        c.env("PYTHONUTF8", "1").env("PYTHONIOENCODING", "utf-8");
        c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        #[cfg(windows)]
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        c
    }

    async fn version(&self) -> String {
        self.yt_dlp.version().await.unwrap_or_else(|| "version unknown".to_string())
    }

    /// Runs a listing command and parses one JSON object per output line.
    async fn list(&self, cmd: Command) -> Result<Vec<Item>> {
        self.list_with(cmd, parse_entry).await
    }

    /// Like `list`, with the parser of the service that answers.
    pub(crate) async fn list_with(&self, mut cmd: Command, parse: fn(&str) -> Result<Option<Item>>) -> Result<Vec<Item>> {
        let out = cmd.output().await.map_err(|e| ProviderError::Io(e.to_string()))?;
        if !out.status.success() {
            return Err(ProviderError::Tool(last_error_line(&String::from_utf8_lossy(&out.stderr))));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut items = Vec::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            if let Some(item) = parse(line)? {
                items.push(item);
            }
        }
        Ok(items)
    }
}

#[derive(Deserialize)]
struct Entry {
    id: Option<String>,
    title: Option<String>,
    uploader: Option<String>,
    channel: Option<String>,
    duration: Option<f64>,
    url: Option<String>,
    webpage_url: Option<String>,
}

/// One line of yt-dlp JSON. Entries without an id are skipped.
fn parse_entry(line: &str) -> Result<Option<Item>> {
    let e: Entry = serde_json::from_str(line).map_err(|e| ProviderError::Parse(e.to_string()))?;
    let Some(id) = e.id.filter(|i| !i.is_empty()) else { return Ok(None) };
    let url = e
        .webpage_url
        .or(e.url.filter(|u| u.starts_with("http")))
        .unwrap_or_else(|| format!("https://www.youtube.com/watch?v={id}"));
    Ok(Some(Item {
        provider: "youtube",
        title: e.title.unwrap_or_else(|| id.clone()),
        artist: e.channel.or(e.uploader).unwrap_or_default(),
        duration_secs: e.duration.filter(|d| *d >= 0.0).map(|d| d as u32),
        url,
        thumbnail: Some(meowtify_core::youtube_thumbnail(&id)),
        id,
    }))
}

const MUSIC_SEARCH_URL: &str = "https://music.youtube.com/youtubei/v1/search?prettyPrint=false";
const MUSIC_CLIENT_VERSION: &str = "1.20240724.01.00";

/// Songs-only search through the YouTube Music web API. About 0.5 s, against 2 s or
/// more for yt-dlp, and the answer already carries artist and length.
pub(crate) async fn music_search(q: &str, limit: usize) -> Result<Vec<Item>> {
    let body = serde_json::json!({
        "context": { "client": { "clientName": "WEB_REMIX", "clientVersion": MUSIC_CLIENT_VERSION, "hl": "en", "gl": "US" } },
        "query": q,
        "params": SONGS_FILTER,
    })
    .to_string();
    let out = crate::ytdlp::curl_post_json(MUSIC_SEARCH_URL, &body, 8).await?;
    parse_music_search(&out, limit)
}

/// Reads the song rows of a YouTube Music search answer. Rows without a video id are skipped.
fn parse_music_search(body: &[u8], limit: usize) -> Result<Vec<Item>> {
    use serde_json::Value;
    let v: Value = serde_json::from_slice(body).map_err(|e| ProviderError::Parse(e.to_string()))?;
    let sections = v
        .pointer("/contents/tabbedSearchResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::Parse("unexpected search answer".into()))?;
    let runs = |col: Option<&Value>| -> Vec<String> {
        col.and_then(|c| c.pointer("/musicResponsiveListItemFlexColumnRenderer/text/runs"))
            .and_then(Value::as_array)
            .map(|r| r.iter().filter_map(|x| x.get("text")?.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    };
    let mut items = Vec::with_capacity(limit);
    'outer: for sec in sections {
        let Some(rows) = sec.pointer("/musicShelfRenderer/contents").and_then(Value::as_array) else { continue };
        for row in rows {
            let Some(r) = row.get("musicResponsiveListItemRenderer") else { continue };
            let Some(id) = r.pointer("/playlistItemData/videoId").and_then(Value::as_str).filter(|i| !i.is_empty())
            else {
                continue;
            };
            let cols = r.get("flexColumns").and_then(Value::as_array);
            let title = runs(cols.and_then(|c| c.first())).concat();
            let sub = runs(cols.and_then(|c| c.get(1)));
            if title.is_empty() {
                continue;
            }
            // Second line: "Artist • Album • 2:42". Artist may span several runs.
            let artist = sub.iter().take_while(|t| t.trim() != "•").map(String::as_str).collect::<String>();
            let duration_secs = sub.last().and_then(|t| parse_clock(t));
            items.push(Item {
                provider: "youtube",
                id: id.to_string(),
                title,
                artist,
                duration_secs,
                url: format!("https://www.youtube.com/watch?v={id}"),
                thumbnail: Some(meowtify_core::youtube_thumbnail(&id)),
            });
            if items.len() >= limit {
                break 'outer;
            }
        }
    }
    Ok(items)
}

/// "2:42" or "1:02:03" in seconds.
pub(crate) fn parse_clock(t: &str) -> Option<u32> {
    let mut total = 0u32;
    let mut parts = 0;
    for p in t.trim().split(':') {
        total = total.checked_mul(60)?.checked_add(p.parse::<u32>().ok()?)?;
        parts += 1;
    }
    (2..=3).contains(&parts).then_some(total)
}

/// Percent-encodes everything except unreserved characters.
pub(crate) fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Words that mark a video upload rather than the song itself.
const VIDEO_MARKS: [&str; 14] = [
    "official video",
    "official music video",
    "music video",
    "lyric video",
    "lyrics",
    "(lyric",
    "visualizer",
    "visualiser",
    "m/v",
    "(mv)",
    "[mv]",
    "full album",
    "reaction",
    "live",
];

/// Higher is more likely a song-only upload. Auto-generated "Topic" channels are
/// the audio of the studio track, so they win. Video-style titles lose.
fn song_score(it: &Item) -> i32 {
    let title = it.title.to_lowercase();
    let mut score = 0;
    if it.artist.ends_with(" - Topic") {
        score += 4;
    }
    if title.contains("official audio") || title.contains("(audio)") || title.contains("[audio]") {
        score += 2;
    }
    if VIDEO_MARKS.iter().any(|m| title.contains(m)) {
        score -= 4;
    }
    if it.duration_secs.is_some_and(|d| d > 600) {
        score -= 2;
    }
    score
}

/// Song-only results first, filled in with what the ordinary search found.
/// `songs` come from the YouTube Music "songs" filter and carry no artist or
/// length, so those are copied from the matching ordinary result when there is one.
/// Ordinary results are added only when there are too few songs, best audio first,
/// so a video is shown only when no song exists.
fn merge(songs: Vec<Item>, mut general: Vec<Item>, limit: usize) -> Vec<Item> {
    const ENOUGH: usize = 5;
    let mut out: Vec<Item> = Vec::with_capacity(limit);
    for mut s in songs {
        if let Some(g) = general.iter().find(|g| g.id == s.id) {
            s.artist = g.artist.trim_end_matches(" - Topic").to_string();
            s.duration_secs = g.duration_secs;
        }
        out.push(s);
    }
    if out.len() < ENOUGH {
        general.sort_by_key(|g| std::cmp::Reverse(song_score(g))); // stable
        for mut g in general {
            if out.len() >= limit {
                break;
            }
            if !out.iter().any(|o| o.id == g.id) {
                g.artist = g.artist.trim_end_matches(" - Topic").to_string();
                out.push(g);
            }
        }
    }
    out.truncate(limit);
    out
}

/// Accepts only https links to YouTube hosts, without credentials or control
/// characters. Returns the link unchanged.
pub fn validate_youtube_url(s: &str) -> Result<String> {
    let s = s.trim();
    let bad = |m: &str| Err(ProviderError::Invalid(m.to_string()));
    if s.is_empty() || s.len() > 2048 || s.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return bad("that is not a valid link");
    }
    let Some(rest) = s.strip_prefix("https://") else { return bad("only https links are accepted") };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return bad("links with a username are not accepted");
    }
    let host = authority.split(':').next().unwrap_or("").to_ascii_lowercase();
    if !HOSTS.contains(&host.as_str()) {
        return bad("only YouTube links are supported");
    }
    Ok(s.to_string())
}

/// The newest finished file in `dir` whose name carries `[id].`. Partial files are ignored.
fn find_saved(dir: &Path, id: &str) -> Option<PathBuf> {
    let needle = format!("[{id}].");
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.contains(&needle) && !name.ends_with(".part") && !name.ends_with(".ytdl") && !name.contains(".temp.")
        })
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p)
}

fn last_error_line(stderr: &str) -> String {
    stderr
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.trim_start_matches("ERROR: ").to_string())
        .unwrap_or_else(|| "yt-dlp failed".to_string())
}

fn parse_progress(line: &str) -> Option<f32> {
    let v = line.strip_prefix(PROGRESS_PREFIX)?.trim().trim_end_matches('%').trim();
    v.parse::<f32>().ok().map(|p| p.clamp(0.0, 100.0))
}

#[async_trait]
impl Provider for YouTube {
    async fn search(&self, query: &str, limit: u32) -> Result<Vec<Item>> {
        let q = query.trim();
        if q.is_empty() || q.chars().count() > MAX_QUERY || q.chars().any(|c| c.is_control()) {
            return Err(ProviderError::Invalid("enter a search of 1 to 200 characters".into()));
        }
        let n = limit.clamp(1, MAX_SEARCH);

        // Fast path: one request to YouTube Music, no yt-dlp start-up. Any failure
        // or an empty answer falls through to the slow path below.
        if let Ok(items) = music_search(q, n as usize).await {
            if !items.is_empty() && (items.len() >= n as usize || n <= FAST_PAGE) {
                return Ok(items);
            }
        }

        // Two searches at once, so the slower one sets the time. The YouTube Music
        // "songs" filter gives song-only results. The ordinary search supplies
        // artist and length, and the fallback when no song matches.
        let mut songs_cmd = self.command();
        songs_cmd.args(["--flat-playlist", "--dump-json", "--skip-download", "--playlist-end", &n.to_string(), "--"]);
        songs_cmd.arg(format!("https://music.youtube.com/search?q={}&sp={SONGS_FILTER}", url_encode(q)));
        let mut general_cmd = self.command();
        general_cmd.args(["--flat-playlist", "--dump-json", "--skip-download"]);
        general_cmd.arg(format!("ytsearch{}:{q}", GENERAL_COUNT.max(n)));

        let (songs, general) = tokio::join!(self.list(songs_cmd), self.list(general_cmd));
        match (songs, general) {
            (Err(e), Err(_)) => Err(e),
            (songs, general) => Ok(merge(songs.unwrap_or_default(), general.unwrap_or_default(), n as usize)),
        }
    }

    async fn resolve(&self, url: &str) -> Result<Vec<Item>> {
        let url = validate_youtube_url(url)?;
        let mut cmd = self.command();
        cmd.args(["--flat-playlist", "--dump-json", "--skip-download", "--playlist-end", "200", "--"]);
        cmd.arg(url);
        self.list(cmd).await
    }

    async fn prefetch(&self, item: &Item) {
        if let Ok(url) = validate_youtube_url(&item.url) {
            self.info_ahead(&item.id, &url).await;
        }
    }

    fn cancel_prefetch(&self) {
        self.info.supersede();
    }

    async fn stream(&self, item: &Item, dest_dir: &Path, progress: ProgressFn, ready: ReadyFn) -> Result<PathBuf> {
        use crate::stream::{fetch, read_source, Failure};
        let url = validate_youtube_url(&item.url)?;
        std::fs::create_dir_all(dest_dir).map_err(|e| ProviderError::Io(e.to_string()))?;
        // The lookup is usually done already (hover prefetch), so this is quick.
        if let Some(source) = self.info_for(&item.id, &url).await.and_then(|info| read_source(&info)) {
            let result = fetch(&source, item, dest_dir, progress.clone(), &*ready).await;
            self.info.forget(&item.id);
            match result {
                Ok(path) => return Ok(path),
                Err(Failure::Late(e)) => return Err(e),
                // Nothing was announced, so the ordinary download below is safe.
                Err(Failure::Early) => {}
            }
        }
        self.download(item, dest_dir, progress).await
    }

    async fn finish(&self, item: &Item, path: &Path) -> Result<()> {
        match &self.tools.ffmpeg {
            Some(ffmpeg) => crate::stream::retag(ffmpeg, item, path).await,
            None => Ok(()), // without ffmpeg, downloads carry no tags either
        }
    }

    async fn download(&self, item: &Item, dest_dir: &Path, progress: ProgressFn) -> Result<PathBuf> {
        let url = validate_youtube_url(&item.url)?;
        std::fs::create_dir_all(dest_dir).map_err(|e| ProviderError::Io(e.to_string()))?;
        // With the lookup done ahead (or done now), yt-dlp starts downloading at once.
        let info = self.info_for(&item.id, &url).await;
        let result = self.run_download(item, &url, dest_dir, progress.clone(), info.as_deref()).await;
        match (&result, info) {
            (Ok(_), _) => self.info.forget(&item.id),
            (Err(_), Some(_)) => {
                // The saved info may have gone stale. Try once the ordinary way.
                self.info.forget(&item.id);
                return self.run_download(item, &url, dest_dir, progress, None).await;
            }
            _ => {}
        }
        result
    }
}

/// The next line of a program's output. A byte that is not UTF-8 (a title in a legacy code page)
/// becomes a replacement character instead of ending the read with an error, which would fail
/// the whole download after the song was already saved. The saved file is found by its id then.
async fn next_line_lossy<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> std::io::Result<Option<String>> {
    let mut buf = Vec::new();
    if reader.read_until(0x0a, &mut buf).await? == 0 {
        return Ok(None);
    }
    while matches!(buf.last(), Some(0x0a | 0x0d)) {
        buf.pop();
    }
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

impl YouTube {
    pub(crate) async fn run_download(
        &self,
        item: &Item,
        url: &str,
        dest_dir: &Path,
        progress: ProgressFn,
        info: Option<&Path>,
    ) -> Result<PathBuf> {
        let mut cmd = self.command();
        cmd.args(["--no-playlist", "--windows-filenames", "--trim-filenames", "200", "--newline", "--progress", "--no-simulate"]);
        cmd.args(["--progress-template", &format!("download:{PROGRESS_PREFIX}%(progress._percent_str)s")]);
        cmd.args(["--print", &format!("after_move:{FILE_PREFIX}%(filepath)s")]);
        cmd.arg("-o").arg(dest_dir.join("%(title).120B [%(id)s].%(ext)s"));
        match &self.tools.ffmpeg {
            Some(ffmpeg) => {
                // m4a (AAC) is what the player decodes. Convert only when the source is not already m4a.
                cmd.args(["-f", "bestaudio[ext=m4a]/bestaudio", "-x", "--audio-format", "m4a", "--audio-quality", "0"]);
                cmd.args(["--embed-metadata"]);
                cmd.arg("--ffmpeg-location").arg(ffmpeg);
            }
            None => {
                cmd.args(["-f", "bestaudio[ext=m4a]"]);
            }
        }
        match info {
            Some(file) => cmd.arg("--load-info-json").arg(file),
            None => cmd.arg("--").arg(url),
        };

        let mut child = cmd.spawn().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => ProviderError::ToolMissing("yt-dlp"),
            _ => ProviderError::Io(e.to_string()),
        })?;
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");

        // yt-dlp writes progress to stderr when it prints values with --print.
        // Read stderr for progress lines and keep its tail for the error message.
        let err_progress = progress.clone();
        let err_task = tokio::spawn(async move {
            let mut tail: Vec<String> = Vec::new();
            let mut lines = BufReader::new(stderr);
            while let Ok(Some(l)) = next_line_lossy(&mut lines).await {
                if let Some(p) = parse_progress(&l) {
                    err_progress(p);
                    continue;
                }
                tail.push(l);
                if tail.len() > 10 {
                    tail.remove(0);
                }
            }
            tail.join("
")
        });

        let mut file: Option<PathBuf> = None;
        let mut lines = BufReader::new(stdout);
        while let Some(line) = next_line_lossy(&mut lines).await.map_err(|e| ProviderError::Io(e.to_string()))? {
            if let Some(p) = parse_progress(&line) {
                progress(p);
            } else if let Some(path) = line.strip_prefix(FILE_PREFIX) {
                file = Some(PathBuf::from(path.trim()));
            }
        }
        let status = child.wait().await.map_err(|e| ProviderError::Io(e.to_string()))?;
        let stderr_text = err_task.await.unwrap_or_default();
        if !status.success() {
            let mut msg = last_error_line(&stderr_text);
            if msg.contains("not available on this app") {
                msg.push_str(" (yt-dlp is probably out of date; update it)");
            }
            // Say which program ran, so a wrong or stale copy is easy to spot.
            msg.push_str(&format!(" [{} {}]", self.yt_dlp.path().display(), self.version().await));
            return Err(ProviderError::Tool(msg));
        }
        // Trust the reported path only if it exists; otherwise look for the file by its video id.
        let path = match file.filter(|p| p.is_file()) {
            Some(p) => p,
            None => find_saved(dest_dir, &item.id)
                .ok_or_else(|| ProviderError::Parse("yt-dlp finished but the saved file was not found".into()))?,
        };
        progress(100.0);
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn output_with_a_stray_byte_is_still_read() {
        let mut data: &[u8] = b"after_move:C:/m/You\x92re Under Arrest [abc].m4a\r\nnext\n";
        let first = super::next_line_lossy(&mut data).await.unwrap().unwrap();
        assert!(first.starts_with("after_move:") && first.ends_with("[abc].m4a"), "{first}");
        assert_eq!(super::next_line_lossy(&mut data).await.unwrap().as_deref(), Some("next"));
        assert_eq!(super::next_line_lossy(&mut data).await.unwrap(), None);
    }

    use super::*;

    fn it(id: &str, title: &str, artist: &str, secs: u32) -> Item {
        Item {
            provider: "youtube",
            id: id.into(),
            title: title.into(),
            artist: artist.into(),
            duration_secs: Some(secs),
            url: String::new(),
            thumbnail: None,
        }
    }

    #[test]
    fn lookups_ahead_of_time_wait_in_a_bounded_queue() {
        let info = InfoCache::new();
        let places: Vec<_> = std::iter::from_fn(|| info.admit()).collect();
        assert_eq!(places.len(), INFO_PARALLEL + INFO_QUEUE, "the queue has a fixed size");
        assert!(info.admit().is_none(), "a full queue drops the request instead of growing");
        drop(places);
        assert!(info.admit().is_some(), "places come back when lookups end");
    }

    #[test]
    fn a_new_search_drops_the_lookups_that_waited_through_it() {
        let info = InfoCache::new();
        let before = info.admit().unwrap();
        assert!(info.wanted(&before));
        info.supersede();
        assert!(!info.wanted(&before), "its row is gone");
        assert!(info.wanted(&info.admit().unwrap()), "later requests are unaffected");
    }

    #[test]
    fn busy_entries_cannot_be_evicted_but_the_map_shrinks_back_once_they_are_free() {
        let info = InfoCache::new();
        // Forty lookups in flight: each holds its cell, so none can go.
        let held: Vec<_> = (0..40).map(|i| info.cell(&format!("busy{i}"))).collect();
        assert_eq!(info.entries.lock().unwrap().len(), 40, "in-use entries are never dropped under a running lookup");
        drop(held);
        // The next cells make room by dropping idle ones, until the map is at its size again.
        info.cell("next");
        let n = info.entries.lock().unwrap().len();
        assert!(n <= INFO_KEEP, "{n} entries stay after the lookups ended");
        assert!(info.has("next"));
    }

    /// Needs the network and yt-dlp: `cargo test -p meowtify-provider live_prefetch -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_prefetch_makes_the_download_start_faster() {
        let tools_dir = PathBuf::from(std::env::var_os("APPDATA").unwrap()).join("Meowtify").join("tools");
        let yt = YouTube::new(Tools::locate_or_default(&tools_dir));
        let item = Item {
            provider: "youtube",
            id: "P2oz2RBtjWQ".into(),
            title: "t".into(),
            artist: String::new(),
            duration_secs: None,
            url: "https://www.youtube.com/watch?v=P2oz2RBtjWQ".into(),
            thumbnail: None,
        };
        let dest = std::env::temp_dir().join(format!("meowtify-dl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dest);
        let progress: ProgressFn = Arc::new(|_| {});
        let t = Instant::now();
        yt.prefetch(&item).await;
        let looked_up = t.elapsed();
        let t = Instant::now();
        let path = yt.download(&item, &dest, progress.clone()).await.unwrap();
        let downloaded = t.elapsed();
        eprintln!("prefetch {looked_up:?}, then download {downloaded:?}: {}", path.display());
        assert!(path.is_file());
        let _ = std::fs::remove_dir_all(&dest);
    }

    /// Needs the network, yt-dlp and curl: `cargo test -p meowtify-provider live_stream -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_stream_reports_ready_before_the_file_is_complete() {
        let tools_dir = PathBuf::from(std::env::var_os("APPDATA").unwrap()).join("Meowtify").join("tools");
        let yt = YouTube::new(Tools::locate_or_default(&tools_dir));
        let item = Item {
            provider: "youtube",
            id: "dQw4w9WgXcQ".into(),
            title: "stream test".into(),
            artist: String::new(),
            duration_secs: None,
            url: "https://www.youtube.com/watch?v=dQw4w9WgXcQ".into(),
            thumbnail: None,
        };
        let dest = std::env::temp_dir().join(format!("meowtify-stream-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dest);
        yt.prefetch(&item).await;
        let t = Instant::now();
        let ready_at = Arc::new(Mutex::new(None));
        let ready: ReadyFn = {
            let ready_at = ready_at.clone();
            Arc::new(move |p: &Path| {
                let g = meowtify_core::growth::lookup(p);
                *ready_at.lock().unwrap() = Some((t.elapsed(), g.map(|g| (g.written(), g.is_done()))));
            })
        };
        let path = yt.stream(&item, &dest, Arc::new(|_| {}), ready).await.unwrap();
        let total = t.elapsed();
        let size = std::fs::metadata(&path).unwrap().len();
        eprintln!("ready {:?}, complete {total:?}, {size} bytes: {}", ready_at.lock().unwrap(), path.display());
        assert!(ready_at.lock().unwrap().is_some(), "ready was never called");
        assert!(size > 100_000);
        let _ = std::fs::remove_dir_all(&dest);
    }

    #[test]
    fn music_search_rows_are_parsed() {
        let row = |id: &str, title: &str, sub: &[&str]| {
            let runs = |t: &[&str]| t.iter().map(|x| serde_json::json!({ "text": x })).collect::<Vec<_>>();
            serde_json::json!({ "musicResponsiveListItemRenderer": {
                "playlistItemData": { "videoId": id },
                "flexColumns": [
                    { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": runs(&[title]) } } },
                    { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": runs(sub) } } },
                ]
            }})
        };
        let body = serde_json::json!({ "contents": { "tabbedSearchResultsRenderer": { "tabs": [ { "tabRenderer": {
            "content": { "sectionListRenderer": { "contents": [ { "musicShelfRenderer": { "contents": [
                row("id1", "laced up", &["tsubi club", " • ", "laced up", " • ", "2:50"]),
                row("id2", "two", &["A", " & ", "B", " • ", "1:02:03"]),
                serde_json::json!({ "musicResponsiveListItemRenderer": {} }),
                row("id3", "three", &["C", " • ", "0:10"]),
            ] } } ] } } } } ] } } })
        .to_string();
        let items = parse_music_search(body.as_bytes(), 2).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!((items[0].id.as_str(), items[0].artist.as_str(), items[0].duration_secs), ("id1", "tsubi club", Some(170)));
        assert_eq!((items[1].artist.as_str(), items[1].duration_secs), ("A & B", Some(3723)));
        assert!(parse_music_search(b"{}", 5).is_err());
    }

    #[test]
    fn clock_text_becomes_seconds() {
        assert_eq!(parse_clock("2:42"), Some(162));
        assert_eq!(parse_clock("1:00:00"), Some(3600));
        assert_eq!(parse_clock("124K plays"), None);
        assert_eq!(parse_clock("laced up"), None);
    }

    /// Needs the network: `cargo test -p meowtify-provider live_music_search -- --ignored`
    #[tokio::test]
    #[ignore]
    async fn live_music_search() {
        let t = std::time::Instant::now();
        let items = music_search("tsubi club", 15).await.unwrap();
        eprintln!("{} items in {:?}: {:?}", items.len(), t.elapsed(), items.first());
        assert!(items.len() >= 5);
        assert!(items.iter().all(|i| !i.artist.is_empty() && i.duration_secs.is_some()));
    }

    #[test]
    fn topic_audio_beats_video_uploads() {
        let topic = it("a", "laced up", "tsubi club - Topic", 180);
        let video = it("b", "tsubi club - laced up (Official Music Video)", "tsubi club", 200);
        let plain = it("c", "laced up", "tsubi club", 180);
        assert!(song_score(&topic) > song_score(&plain));
        assert!(song_score(&plain) > song_score(&video));
        assert!(song_score(&it("d", "laced up [full album]", "x", 1700)) < song_score(&plain));
    }

    #[test]
    fn songs_come_first_with_artist_copied_over() {
        let songs = vec![it("a", "laced up", "", 0), it("b", "burbank house", "", 0)];
        let general = vec![
            it("v", "laced up (official video)", "tsubi club", 200),
            it("b", "burbank house", "tsubi club - Topic", 113),
        ];
        let out = merge(songs, general, 15);
        assert_eq!(out[0].id, "a");
        assert_eq!(out[1].artist, "tsubi club");
        assert_eq!(out[1].duration_secs, Some(113));
        assert!(out.iter().any(|o| o.id == "v"), "too few songs: fall back to videos");
    }

    #[test]
    fn enough_songs_hide_videos() {
        let songs: Vec<Item> = (0..6).map(|i| it(&format!("s{i}"), "song", "", 0)).collect();
        let out = merge(songs, vec![it("v", "x (official video)", "y", 100)], 15);
        assert_eq!(out.len(), 6);
    }

    #[test]
    fn falls_back_to_best_audio_when_no_songs() {
        let general = vec![it("v", "x (official video)", "y", 100), it("t", "x", "y - Topic", 100)];
        let out = merge(Vec::new(), general, 15);
        assert_eq!(out[0].id, "t");
        assert_eq!(out[0].artist, "y");
    }

    #[test]
    fn query_is_percent_encoded() {
        assert_eq!(url_encode("a b&c=d"), "a%20b%26c%3Dd");
    }

    #[test]
    fn accepts_youtube_links_only() {
        for ok in [
            "https://www.youtube.com/watch?v=K0HSD_i2DvA",
            "https://youtu.be/K0HSD_i2DvA",
            "https://music.youtube.com/playlist?list=PL123",
            "  https://m.youtube.com/watch?v=x  ",
            "https://WWW.YouTube.com:443/watch?v=x",
        ] {
            assert!(validate_youtube_url(ok).is_ok(), "should accept {ok}");
        }
        for bad in [
            "",
            "http://www.youtube.com/watch?v=x",
            "https://evil.com/watch?v=x",
            "https://www.youtube.com.evil.com/x",
            "https://user@www.youtube.com/x",
            "https://evil.com#@www.youtube.com/x",
            "https://www.youtube.com/x y",
            "https://www.youtube.com/\nx",
            "--exec calc",
            "file:///C:/x",
            "ytsearch5:cats",
        ] {
            assert!(validate_youtube_url(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn parses_entries() {
        let line = r#"{"id":"abc","title":"T","channel":"C","uploader":"U","duration":241.6,"url":"https://www.youtube.com/watch?v=abc"}"#;
        let i = parse_entry(line).unwrap().unwrap();
        assert_eq!((i.id.as_str(), i.title.as_str(), i.artist.as_str()), ("abc", "T", "C"));
        assert_eq!(i.duration_secs, Some(241));
        assert_eq!(i.url, "https://www.youtube.com/watch?v=abc");

        let minimal = parse_entry(r#"{"id":"zzz"}"#).unwrap().unwrap();
        assert_eq!(minimal.title, "zzz");
        assert_eq!(minimal.url, "https://www.youtube.com/watch?v=zzz");

        assert!(parse_entry(r#"{"title":"no id"}"#).unwrap().is_none());
        assert!(parse_entry("not json").is_err());
    }

    #[test]
    fn finds_saved_file_by_video_id_even_with_unicode_names() {
        let dir = std::env::temp_dir().join(format!("meowtify-yt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("( ╥﹏╥ ) [abc123].m4a"), b"x").unwrap();
        std::fs::write(dir.join("other [zzz].m4a"), b"x").unwrap();
        std::fs::write(dir.join("( ╥﹏╥ ) [abc123].m4a.part"), b"x").unwrap();
        let found = find_saved(&dir, "abc123").unwrap();
        assert!(found.to_string_lossy().ends_with("[abc123].m4a"));
        assert!(find_saved(&dir, "nope").is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parses_progress_and_errors() {
        assert_eq!(parse_progress("MEOWPROGRESS  12.3%"), Some(12.3));
        assert_eq!(parse_progress("MEOWPROGRESS 100%"), Some(100.0));
        assert_eq!(parse_progress("MEOWPROGRESS NA"), None);
        assert_eq!(parse_progress("something else"), None);
        assert_eq!(last_error_line("x\nERROR: Video unavailable\n\n"), "Video unavailable");
        assert_eq!(last_error_line(""), "yt-dlp failed");
    }

    #[tokio::test]
    async fn search_rejects_bad_queries_before_running_anything() {
        let yt = YouTube::new(Tools { yt_dlp: PathBuf::from("does-not-exist"), ffmpeg: None });
        assert!(matches!(yt.search("   ", 5).await, Err(ProviderError::Invalid(_))));
        assert!(matches!(yt.search("a\nb", 5).await, Err(ProviderError::Invalid(_))));
        assert!(matches!(yt.search(&"x".repeat(300), 5).await, Err(ProviderError::Invalid(_))));
        assert!(matches!(yt.resolve("https://evil.com/x").await, Err(ProviderError::Invalid(_))));
    }
}
