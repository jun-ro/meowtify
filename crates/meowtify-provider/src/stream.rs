//! Progressive download for streaming playback. The direct audio link comes from the
//! info file that yt-dlp already wrote, and `curl` fetches it in ranges into the final
//! file while the player reads the front of that file.
//!
//! Only plain https m4a is handled. Anything else (HLS, Opus in webm) falls back to the
//! ordinary yt-dlp download, because the player cannot decode it from a growing file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use meowtify_core::growth::{self, Growth};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::youtube::plain_command;
use crate::{Item, ProgressFn, ProviderError, Result};

/// Bytes on disk before the player is told it can start. About 4 s of 130 kbit/s AAC.
pub(crate) const READY_BYTES: u64 = 64 * 1024;
/// Size of one range request. YouTube throttles very large single requests.
const CHUNK: u64 = 4 * 1024 * 1024;
/// A chunk request that takes longer than this is abandoned.
const CHUNK_TIMEOUT: Duration = Duration::from_secs(120);

/// What the info file says about the audio stream.
#[derive(Debug, Deserialize)]
struct Info {
    url: Option<String>,
    ext: Option<String>,
    protocol: Option<String>,
    filesize: Option<u64>,
    filesize_approx: Option<f64>,
    http_headers: Option<HashMap<String, String>>,
}

/// A stream that can be fetched directly.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Source {
    pub url: String,
    /// The exact length, when the info file states it. Nothing else may end the download.
    pub size: Option<u64>,
    /// An estimate, only for the progress bar. It is not where the file ends.
    pub approx: Option<u64>,
    pub user_agent: Option<String>,
}

/// Reads the info file at `path`. `None` when the format cannot be streamed.
pub(crate) fn read_source(path: &Path) -> Option<Source> {
    let bytes = std::fs::read(path).ok()?;
    parse_source(&bytes)
}

fn parse_source(bytes: &[u8]) -> Option<Source> {
    let info: Info = serde_json::from_slice(bytes).ok()?;
    if info.ext.as_deref() != Some("m4a") || info.protocol.as_deref().is_some_and(|p| p != "https") {
        return None;
    }
    let url = info.url?;
    if !is_media_url(&url) {
        return None;
    }
    let size = info.filesize.filter(|&s| s > 0);
    let approx = info.filesize_approx.filter(|f| f.is_finite() && *f > 0.0).map(|f| f as u64);
    let user_agent = info
        .http_headers
        .and_then(|h| h.into_iter().find(|(k, _)| k.eq_ignore_ascii_case("user-agent")).map(|(_, v)| v))
        .filter(|ua| ua.len() <= 300 && ua.chars().all(|c| !c.is_control()));
    Some(Source { url, size, approx, user_agent })
}

/// YouTube serves audio from `*.googlevideo.com` over https. Anything else is refused,
/// so a changed info file cannot point curl at another host.
fn is_media_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else { return false };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    !host.contains(['@', ':']) && host.ends_with(".googlevideo.com") && url.chars().all(|c| c.is_ascii() && !c.is_control())
}

/// File name in the library style, `Title [id].m4a`. Characters Windows forbids become `_`.
pub(crate) fn file_name(item: &Item) -> String {
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
    format!("{title} [{}].m4a", item.id)
}

/// Removes a half-written file when the download does not finish, and tells readers.
struct Cleanup {
    path: PathBuf,
    growth: Arc<Growth>,
    armed: bool,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if self.armed {
            self.growth.fail();
            growth::forget(&self.path);
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Fetches `source` into `dest_dir`. Calls `ready` once `READY_BYTES` are on disk.
/// Returns the path when the whole file is saved. `Err(Early)` means nothing was
/// announced yet, so the caller may fall back to another way.
pub(crate) async fn fetch(
    source: &Source,
    item: &Item,
    dest_dir: &Path,
    progress: ProgressFn,
    ready: &(dyn Fn(&Path) + Send + Sync),
) -> std::result::Result<PathBuf, Failure> {
    let path = dest_dir.join(file_name(item));
    let growth = Growth::new();
    let file = tokio::fs::File::create(&path).await.map_err(|_| Failure::Early)?;
    growth::register(&path, growth.clone());
    let mut guard = Cleanup { path: path.clone(), growth: growth.clone(), armed: true };
    let mut file = file;

    let mut offset: u64 = 0;
    let mut announced = false;
    // The exact length: from the info file, then replaced by what the server states.
    let mut exact = source.size;
    let header_file = path.with_extension("hdr");
    let _header_cleanup = RemoveOnDrop(header_file.clone());
    loop {
        if exact.is_some_and(|size| offset >= size) {
            break;
        }
        let want = exact.map_or(CHUNK, |size| CHUNK.min(size - offset));
        let hint = exact.or(source.approx);
        let got = match tokio::time::timeout(
            CHUNK_TIMEOUT,
            fetch_chunk(source, offset, want, &header_file, &mut file, &growth, &mut |written| {
                if let Some(total) = hint {
                    // Only the real end of the file may show 100.
                    let top = if exact.is_some() { 100.0 } else { 99.0 };
                    progress((written as f64 / total as f64 * 100.0).min(top) as f32);
                }
                // Tell the player as soon as enough is on disk, not when the chunk ends.
                if !announced && written >= READY_BYTES {
                    announced = true;
                    ready(&path);
                }
            }),
        )
        .await
        {
            Ok(r) => r,
            Err(_) => Err(ProviderError::Tool("the download timed out".into())),
        };
        let chunk = match got {
            Ok(c) => c,
            // The file ended exactly at the previous range.
            Err(ProviderError::Tool(m)) if exact.is_none() && offset > 0 && m.contains("416") => break,
            Err(e) => return Err(if announced { Failure::Late(e) } else { Failure::Early }),
        };
        offset += chunk.bytes;
        if let Some(total) = chunk.total {
            exact = Some(total);
        }
        if chunk.bytes == 0 || (exact.is_none() && chunk.bytes < want) {
            break; // a short answer is the end of the file
        }
    }
    file.flush().await.ok();
    drop(file);
    if let Some(size) = exact {
        if offset != size {
            let e = ProviderError::Tool(format!("the download has {offset} bytes, the server said {size}"));
            return Err(if announced { Failure::Late(e) } else { Failure::Early });
        }
    }
    growth.finish(offset);
    guard.armed = false;
    if !announced {
        ready(&path); // a small file: it is complete and playable
    }
    // Readers that already hold the `Growth` keep it; a later open sees a plain finished file.
    growth::forget(&path);
    progress(100.0);
    Ok(path)
}

/// Removes a file when dropped.
struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// How long [`retag`] keeps trying to replace a file that is still open.
const REPLACE_FOR: Duration = Duration::from_secs(60 * 60);
const REPLACE_EVERY: Duration = Duration::from_secs(2);

/// Metadata text for ffmpeg: no control characters, at most 200 characters.
fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(200).collect::<String>().trim().to_string()
}

/// Rewrites `path` with title and artist tags and the index at the front, without
/// re-encoding. The result is written next to the file and moved over it; while another
/// program (the player) holds the file open the move fails, so this retries. Dropping
/// the future removes the temporary file.
pub(crate) async fn retag(ffmpeg: &Path, item: &Item, path: &Path) -> Result<()> {
    let tmp = path.with_extension("tagging");
    struct Remove(PathBuf);
    impl Drop for Remove {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _remove = Remove(tmp.clone());

    let mut cmd = plain_command(ffmpeg);
    cmd.args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"]).arg(path);
    cmd.args(["-map", "0:a", "-c", "copy", "-map_metadata", "0", "-movflags", "+faststart", "-f", "mp4"]);
    cmd.arg("-metadata").arg(format!("title={}", clean(&item.title)));
    if !item.artist.trim().is_empty() {
        cmd.arg("-metadata").arg(format!("artist={}", clean(&item.artist)));
    }
    cmd.arg(&tmp);
    cmd.stdout(Stdio::null()).stderr(Stdio::piped());
    let out = tokio::time::timeout(Duration::from_secs(120), cmd.output())
        .await
        .map_err(|_| ProviderError::Tool("ffmpeg timed out".into()))?
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => ProviderError::ToolMissing("ffmpeg"),
            _ => ProviderError::Io(e.to_string()),
        })?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        return Err(ProviderError::Tool(msg.lines().last().unwrap_or("ffmpeg failed").trim().to_string()));
    }

    let deadline = tokio::time::Instant::now() + REPLACE_FOR;
    loop {
        match std::fs::rename(&tmp, path) {
            Ok(()) => {
                std::mem::forget(_remove); // the temporary file is gone: it became the song
                return Ok(());
            }
            Err(e) if tokio::time::Instant::now() >= deadline => return Err(ProviderError::Io(e.to_string())),
            // Most likely the player still has the file open. Try again soon.
            Err(_) => tokio::time::sleep(REPLACE_EVERY).await,
        }
    }
}

/// Why a streamed download ended early.
#[derive(Debug)]
pub(crate) enum Failure {
    /// Before the player was told to start. The caller can use another way.
    Early,
    /// After the player started on the file.
    Late(ProviderError),
}

/// What one range request delivered.
#[derive(Debug)]
struct Chunk {
    /// Bytes appended to the file.
    bytes: u64,
    /// The length of the whole file, when the reply states it.
    total: Option<u64>,
}

/// The parts of the reply headers that matter here.
#[derive(Debug, PartialEq, Eq)]
struct Reply {
    status: u16,
    /// `Content-Range`: first byte, last byte, and the whole length when known.
    range: Option<(u64, u64, Option<u64>)>,
}

/// Reads the headers curl dumped. Several blocks (redirects) are possible; the last counts.
fn parse_headers(text: &str) -> Option<Reply> {
    let mut reply: Option<Reply> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.len() >= 5 && line[..5].eq_ignore_ascii_case("HTTP/") {
            let status = line.split_whitespace().nth(1)?.parse().ok()?;
            reply = Some(Reply { status, range: None });
        } else if let (Some(r), Some((name, value))) = (reply.as_mut(), line.split_once(':')) {
            if name.trim().eq_ignore_ascii_case("content-range") {
                r.range = parse_content_range(value.trim());
            }
        }
    }
    reply
}

/// `bytes 0-99/1000` or `bytes 0-99/*`.
fn parse_content_range(value: &str) -> Option<(u64, u64, Option<u64>)> {
    let rest = value.strip_prefix("bytes")?.trim();
    let (span, total) = rest.split_once('/')?;
    let (first, last) = span.split_once('-')?;
    let (first, last): (u64, u64) = (first.trim().parse().ok()?, last.trim().parse().ok()?);
    if last < first {
        return None;
    }
    let total = match total.trim() {
        "*" => None,
        t => Some(t.parse().ok()?),
    };
    Some((first, last, total))
}

/// Runs one curl request for `want` bytes from `offset` and appends what it returns. The
/// reply headers are checked afterwards: a server that ignored the range, or answered with
/// another range, fails the download instead of leaving wrong bytes in the file.
async fn fetch_chunk(
    source: &Source,
    offset: u64,
    want: u64,
    header_file: &Path,
    file: &mut tokio::fs::File,
    growth: &Growth,
    on_progress: &mut (dyn FnMut(u64) + Send),
) -> Result<Chunk> {
    let _ = std::fs::remove_file(header_file);
    let mut cmd = plain_command(Path::new(if cfg!(windows) { "curl.exe" } else { "curl" }));
    cmd.args(["--fail", "--silent", "--show-error", "--proto", "=https", "--max-time", "110"]);
    crate::net::apply(&mut cmd);
    if let Some(ua) = &source.user_agent {
        cmd.args(["-A", ua]);
    }
    cmd.arg("-D").arg(header_file);
    cmd.args(["-r", &format!("{offset}-{}", offset + want - 1)]);
    cmd.args(["--url", &source.url]);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    // A dropped future (timeout, cancel) must not leave curl running.
    cmd.kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => ProviderError::ToolMissing("curl"),
        _ => ProviderError::Io(e.to_string()),
    })?;
    let mut out = child.stdout.take().expect("stdout is piped");
    let mut err = child.stderr.take().expect("stderr is piped");

    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = out.read(&mut buf).await.map_err(|e| ProviderError::Io(e.to_string()))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).await.map_err(|e| ProviderError::Io(e.to_string()))?;
        file.flush().await.map_err(|e| ProviderError::Io(e.to_string()))?;
        total += n as u64;
        growth.set_written(offset + total);
        on_progress(offset + total);
    }
    let status = child.wait().await.map_err(|e| ProviderError::Io(e.to_string()))?;
    if !status.success() {
        let mut msg = String::new();
        let _ = err.read_to_string(&mut msg).await;
        let msg = msg.trim();
        return Err(ProviderError::Tool(if msg.is_empty() { "curl failed".into() } else { msg.to_string() }));
    }
    let headers = std::fs::read(header_file).ok().and_then(|b| parse_headers(&String::from_utf8_lossy(&b)));
    check_reply(headers, offset, total)
}

/// Accepts a reply only when it holds exactly the bytes asked for.
fn check_reply(reply: Option<Reply>, offset: u64, bytes: u64) -> Result<Chunk> {
    let wrong = |m: String| Err(ProviderError::Tool(m));
    let Some(reply) = reply else { return wrong("the server's reply could not be read".into()) };
    match (reply.status, reply.range) {
        (206, Some((first, last, total))) => {
            if first != offset {
                return wrong(format!("the server sent bytes from {first}, not {offset}"));
            }
            if bytes != last - first + 1 {
                return wrong(format!("the server announced {} bytes and sent {bytes}", last - first + 1));
            }
            Ok(Chunk { bytes, total })
        }
        (206, None) => wrong("the server sent a range without saying which".into()),
        // The range was ignored. That is only usable from the start: the body is the whole file.
        (200, _) if offset == 0 => Ok(Chunk { bytes, total: Some(bytes) }),
        (200, _) => wrong("the server ignored the range request".into()),
        (status, _) => wrong(format!("the server answered {status}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: &str) -> Item {
        Item {
            provider: "youtube",
            id: "abc123XYZ_-".into(),
            title: title.into(),
            artist: String::new(),
            duration_secs: None,
            url: String::new(),
            thumbnail: None,
        }
    }

    #[test]
    fn accepts_a_plain_https_m4a() {
        let json = br#"{"url":"https://rr1---sn-x.googlevideo.com/videoplayback?expire=1","ext":"m4a","protocol":"https","filesize":3290000,"http_headers":{"User-Agent":"Mozilla/5.0"}}"#;
        let s = parse_source(json).unwrap();
        assert_eq!(s.size, Some(3_290_000));
        assert_eq!(s.approx, None);
        assert_eq!(s.user_agent.as_deref(), Some("Mozilla/5.0"));
    }

    #[test]
    fn an_estimate_is_not_the_length() {
        let json = br#"{"url":"https://rr1---sn-x.googlevideo.com/videoplayback?expire=1","ext":"m4a","protocol":"https","filesize_approx":3290000.5}"#;
        let s = parse_source(json).unwrap();
        assert_eq!(s.size, None);
        assert_eq!(s.approx, Some(3_290_000));
    }

    #[test]
    fn reads_the_range_of_a_reply() {
        let text = "HTTP/1.1 302 Found\r\nLocation: x\r\n\r\nHTTP/2 206\r\ncontent-length: 100\r\nContent-Range: bytes 100-199/1000\r\n\r\n";
        let r = parse_headers(text).unwrap();
        assert_eq!(r, Reply { status: 206, range: Some((100, 199, Some(1000))) });
        assert_eq!(parse_content_range("bytes 0-9/*"), Some((0, 9, None)));
        assert_eq!(parse_content_range("bytes 9-0/5"), None);
    }

    #[test]
    fn only_the_asked_bytes_are_accepted() {
        let ok = Reply { status: 206, range: Some((100, 199, Some(1000))) };
        assert_eq!(check_reply(Some(ok), 100, 100).unwrap().total, Some(1000));
        let moved = Reply { status: 206, range: Some((0, 99, Some(1000))) };
        assert!(check_reply(Some(moved), 100, 100).is_err());
        let short = Reply { status: 206, range: Some((100, 199, None)) };
        assert!(check_reply(Some(short), 100, 60).is_err());
        let ignored = Reply { status: 200, range: None };
        assert!(check_reply(Some(Reply { status: 200, range: None }), 100, 500).is_err());
        assert_eq!(check_reply(Some(ignored), 0, 500).unwrap().total, Some(500));
        assert!(check_reply(None, 0, 1).is_err());
    }

    #[test]
    fn refuses_other_formats_and_hosts() {
        let m3u8 = br#"{"url":"https://manifest.googlevideo.com/x.m3u8","ext":"mp4","protocol":"m3u8_native"}"#;
        assert!(parse_source(m3u8).is_none());
        let webm = br#"{"url":"https://a.googlevideo.com/x","ext":"webm","protocol":"https"}"#;
        assert!(parse_source(webm).is_none());
        for url in [
            "http://a.googlevideo.com/x",
            "https://evil.example.com/x",
            "https://a.googlevideo.com.evil.com/x",
            "https://user@a.googlevideo.com/x",
            "https://a.googlevideo.com:8443/x",
            "file:///C:/secret",
        ] {
            let json = format!(r#"{{"url":"{url}","ext":"m4a","protocol":"https"}}"#);
            assert!(parse_source(json.as_bytes()).is_none(), "{url}");
        }
    }

    #[test]
    fn file_names_are_safe_and_carry_the_id() {
        assert_eq!(file_name(&item("AC/DC: Back <in> Black?")), "AC_DC_ Back _in_ Black_ [abc123XYZ_-].m4a");
        assert_eq!(file_name(&item("...")), "Unknown [abc123XYZ_-].m4a");
        assert!(file_name(&item(&"x".repeat(500))).chars().count() < 150);
    }

    /// Needs ffmpeg and a real m4a: `MEOWTIFY_TEST_M4A=path MEOWTIFY_TEST_FFMPEG=path cargo test -p meowtify-provider retag_writes -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn retag_writes_tags_without_re_encoding() {
        let src = std::env::var("MEOWTIFY_TEST_M4A").expect("set MEOWTIFY_TEST_M4A");
        let ffmpeg = PathBuf::from(std::env::var("MEOWTIFY_TEST_FFMPEG").expect("set MEOWTIFY_TEST_FFMPEG"));
        let dir = std::env::temp_dir().join(format!("meowtify-retag-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("song [abc].m4a");
        std::fs::copy(&src, &path).unwrap();
        let before = std::fs::metadata(&path).unwrap().len();

        let mut it = item("Tagged Title");
        it.artist = "Some Artist".into();
        retag(&ffmpeg, &it, &path).await.unwrap();

        let after = std::fs::metadata(&path).unwrap().len();
        assert!(!path.with_extension("tagging").exists(), "temporary file left behind");
        let out = std::process::Command::new(&ffmpeg).arg("-hide_banner").arg("-i").arg(&path).output().unwrap();
        let info = String::from_utf8_lossy(&out.stderr).into_owned();
        eprintln!("size {before} -> {after}
{info}");
        assert!(info.contains("Tagged Title") && info.contains("Some Artist"));
        assert!(info.contains("Audio: aac"), "audio was re-encoded or lost");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
