//! Writing audio to disk in small pieces.
//!
//! Nothing in here holds more than a 64 KiB read buffer. A download writes a `.part`
//! file and renames it only when the whole thing arrived, so a cancelled or failed
//! job never leaves a half song in the library.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncSeekExt, AsyncWrite, AsyncWriteExt};

use crate::native::http;
use crate::{ProviderError, ProgressFn, Result};

/// Bytes per range request while a file is streamed. YouTube refuses very large single
/// requests; yt-dlp uses 10 MiB, and 9 MiB keeps each request under a minute on a slow link.
pub(crate) const CHUNK: u64 = 9 * 1024 * 1024;

/// Bytes per range request when a whole song is saved. YouTube gives a fast burst and then
/// throttles a long range to a fraction of the link, so a song is split into small ranges.
const PIECE: u64 = 1024 * 1024;

/// Range requests in flight at once. Each one holds a 64 KiB read buffer and one
/// connection, so this is the whole cost in memory (plus TLS state).
const PARALLEL: usize = 4;

/// A read that takes longer than this means the connection is dead.
const READ_TIMEOUT: Duration = Duration::from_secs(120);

/// Removes a partial file when the download does not reach the end.
pub(crate) struct PartFile {
    path: PathBuf,
    keep: bool,
}

impl PartFile {
    pub fn new(path: PathBuf) -> PartFile {
        PartFile { path, keep: false }
    }

    /// The file is complete: leave it where it is.
    pub fn keep(mut self) {
        self.keep = true;
    }
}

impl Drop for PartFile {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// How many bytes to ask for next. The last request stops at the end of the file.
pub(crate) fn chunk_len(offset: u64, total: Option<u64>) -> u64 {
    match total {
        Some(total) => CHUNK.min(total.saturating_sub(offset)),
        None => CHUNK,
    }
}

/// `bytes 0-99/1000` or `bytes 0-99/*`.
pub(crate) fn parse_content_range(value: &str) -> Option<(u64, u64, Option<u64>)> {
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

/// Bytes written so far, shared by the pieces of one download, and the progress report.
struct Feed {
    written: AtomicU64,
    total: AtomicU64,
    progress: ProgressFn,
}

impl Feed {
    fn add(&self, n: u64) {
        let done = self.written.fetch_add(n, Ordering::Relaxed) + n;
        let total = self.total.load(Ordering::Relaxed);
        if total > 0 {
            (self.progress)((done as f64 / total as f64 * 100.0).min(99.0) as f32);
        }
    }
}

/// What one range request brought back.
struct Piece {
    /// The size the server stated for the whole file, when it did.
    total: Option<u64>,
    /// The server ignored the range and sent the file from byte 0.
    whole: bool,
    /// Bytes written to the part file.
    got: u64,
}

/// A media URL and the headers it needs, for a piece fetched on another task.
#[derive(Clone)]
struct Ranged {
    url: String,
    headers: Vec<(String, String)>,
}

impl Ranged {
    fn new(url: &str, headers: &[(&str, &str)]) -> Ranged {
        Ranged {
            url: url.to_string(),
            headers: headers.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect(),
        }
    }

    /// Fetches `want` bytes at `offset`, overwriting that span of `part`.
    ///
    /// A broken connection is tried once more: the span is written from its start again,
    /// so a half-saved piece cannot leave a hole.
    async fn piece(&self, part: &Path, offset: u64, want: u64, feed: &Feed) -> Result<Piece> {
        match self.piece_once(part, offset, want, feed).await {
            Ok(piece) => Ok(piece),
            Err(e) if http::transient(&e) => {
                tokio::time::sleep(Duration::from_millis(400)).await;
                self.piece_once(part, offset, want, feed).await
            }
            Err(e) => Err(e),
        }
    }

    async fn piece_once(&self, part: &Path, offset: u64, want: u64, feed: &Feed) -> Result<Piece> {
        let headers: Vec<(&str, &str)> = self.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let mut reply = http::get_range(&self.url, offset, want, &headers).await?;
        let whole = match reply.status {
            206 => false,
            200 if offset == 0 => true,
            416 if offset > 0 => return Ok(Piece { total: None, whole: false, got: 0 }),
            status => return Err(ProviderError::Tool(format!("the audio server answered {status}"))),
        };
        let mut total = None;
        if let Some(range) = reply.header("content-range").and_then(parse_content_range) {
            if range.0 != offset {
                return Err(ProviderError::Tool(format!("the server sent bytes from {}, not {offset}", range.0)));
            }
            total = range.2.filter(|t| *t > 0);
        }

        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(part)
            .await
            .map_err(|e| ProviderError::Io(e.to_string()))?;
        file.seek(std::io::SeekFrom::Start(offset)).await.map_err(|e| ProviderError::Io(e.to_string()))?;
        let mut buf = vec![0u8; 64 * 1024];
        let mut got = 0u64;
        loop {
            let n = match tokio::time::timeout(READ_TIMEOUT, reply.read(&mut buf)).await {
                Ok(r) => r?,
                Err(_) => return Err(ProviderError::Tool("the download timed out".into())),
            };
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n]).await.map_err(|e| ProviderError::Io(e.to_string()))?;
            got += n as u64;
            feed.add(n as u64);
            if !whole && got >= want {
                break;
            }
        }
        file.flush().await.map_err(|e| ProviderError::Io(e.to_string()))?;
        Ok(Piece { total, whole, got })
    }
}

/// Downloads `url` with byte ranges, writing to `part`. Returns the file size.
///
/// `total_hint` is the size the API stated; the server's `Content-Range` replaces it.
/// Progress is a percentage of the real length, and stays under 100 until the rename.
///
/// YouTube throttles a single long range after a short burst, which is why a whole song is
/// fetched as small ranges with a few of them in flight; the streaming path never waits for
/// all of it, so it does not feel this. Every piece is written straight to disk at its
/// offset, so memory stays flat however long the song is.
pub(crate) async fn download_ranged(
    url: &str,
    headers: &[(&str, &str)],
    part: &Path,
    total_hint: Option<u64>,
    progress: ProgressFn,
) -> Result<u64> {
    tokio::fs::File::create(part).await.map_err(|e| ProviderError::Io(e.to_string()))?;
    let ranged = Ranged::new(url, headers);
    let mut total = total_hint.filter(|t| *t > 0);
    let feed = Arc::new(Feed {
        written: AtomicU64::new(0),
        total: AtomicU64::new(total.unwrap_or(0)),
        progress,
    });

    // The first piece is asked for on its own: it reports the real length.
    let first_want = total.map_or(CHUNK, |t| PIECE.min(t));
    let first = ranged.piece(part, 0, first_want, &feed).await?;
    if let Some(t) = first.total {
        total = Some(t);
        feed.total.store(t, Ordering::Relaxed);
    }
    if first.whole {
        return checked_size(part, total).await;
    }
    let mut offset = first.got;
    if offset == 0 {
        return Err(ProviderError::Tool("the audio server sent no audio".into()));
    }

    match total {
        Some(total) if offset < total => {
            // The rest in small ranges, up to `PARALLEL` at once. The spans do not overlap,
            // so each task writes its own part of the file without a lock.
            let mut spans = Vec::new();
            let mut at = offset;
            while at < total {
                let len = PIECE.min(total - at);
                spans.push((at, len));
                at += len;
            }
            for group in spans.chunks(PARALLEL) {
                let mut set = tokio::task::JoinSet::new();
                for &(start, len) in group {
                    let (ranged, part, feed) = (ranged.clone(), part.to_path_buf(), feed.clone());
                    set.spawn(async move {
                        let piece = ranged.piece(&part, start, len, &feed).await?;
                        Ok::<(u64, u64, Piece), ProviderError>((start, len, piece))
                    });
                }
                while let Some(joined) = set.join_next().await {
                    let (start, len, piece) =
                        joined.map_err(|e| ProviderError::Tool(format!("a download piece failed: {e}")))??;
                    if piece.whole {
                        return Err(ProviderError::Tool(format!("the server ignored the range at {start}")));
                    }
                    if piece.got != len {
                        return Err(ProviderError::Tool(format!(
                            "the server sent {} bytes from {start}, not {len}",
                            piece.got
                        )));
                    }
                }
            }
        }
        Some(_) => {}
        None => {
            // No length from the server: keep asking until a short answer ends the file.
            loop {
                let piece = ranged.piece(part, offset, CHUNK, &feed).await?;
                offset += piece.got;
                if piece.whole || piece.got < CHUNK {
                    break;
                }
            }
        }
    }

    checked_size(part, total).await
}

/// The size on disk, refused when it disagrees with the length the server stated.
async fn checked_size(part: &Path, total: Option<u64>) -> Result<u64> {
    let size = tokio::fs::metadata(part).await.map(|m| m.len()).unwrap_or(0);
    if let Some(total) = total {
        if size != total {
            return Err(ProviderError::Tool(format!("the download has {size} bytes, the server said {total}")));
        }
    }
    Ok(size)
}

/// Copies the rest of `reply` into `file`, in 64 KiB pieces, and returns the count.
pub(crate) async fn copy_body<W: AsyncWrite + Unpin>(
    reply: &mut http::Response,
    file: &mut W,
    mut on_bytes: impl FnMut(u64),
) -> Result<u64> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = match tokio::time::timeout(READ_TIMEOUT, reply.read(&mut buf)).await {
            Ok(r) => r?,
            Err(_) => return Err(ProviderError::Tool("the download timed out".into())),
        };
        if n == 0 {
            return Ok(total);
        }
        file.write_all(&buf[..n]).await.map_err(|e| ProviderError::Io(e.to_string()))?;
        total += n as u64;
        on_bytes(total);
    }
}

/// Downloads `url` in one request (the CDNs used here allow that), appending to `part`.
pub(crate) async fn download_plain(
    url: &str,
    headers: &[(&str, &str)],
    part: &Path,
    progress: ProgressFn,
) -> Result<u64> {
    let mut reply = http::get(url, headers).await?;
    if reply.status != 200 {
        let status = reply.status;
        let body = reply.bytes(16 * 1024).await.unwrap_or_default();
        return Err(http::status_error("the audio server", status, &body));
    }
    let total = reply.content_length();
    let mut file = tokio::fs::File::create(part).await.map_err(|e| ProviderError::Io(e.to_string()))?;
    let size = copy_body(&mut reply, &mut file, |done| {
        if let Some(total) = total.filter(|t| *t > 0) {
            progress((done as f64 / total as f64 * 100.0).min(99.0) as f32);
        }
    })
    .await?;
    file.flush().await.map_err(|e| ProviderError::Io(e.to_string()))?;
    Ok(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asks_for_at_most_a_chunk_and_stops_at_the_end() {
        assert_eq!(chunk_len(0, None), CHUNK);
        assert_eq!(chunk_len(0, Some(CHUNK * 3)), CHUNK);
        assert_eq!(chunk_len(CHUNK * 2, Some(CHUNK * 3)), CHUNK);
        assert_eq!(chunk_len(CHUNK * 3 - 5, Some(CHUNK * 3)), 5);
        assert_eq!(chunk_len(10, Some(10)), 0);
    }

    #[test]
    fn reads_content_range() {
        assert_eq!(parse_content_range("bytes 0-99/1000"), Some((0, 99, Some(1000))));
        assert_eq!(parse_content_range("bytes 100-199/*"), Some((100, 199, None)));
        assert_eq!(parse_content_range("bytes 9-0/5"), None);
        assert_eq!(parse_content_range("items 0-1/2"), None);
    }

    /// A whole song is fetched as several ranges at once, and every byte lands in place.
    #[tokio::test]
    async fn downloads_a_song_in_parallel_pieces() {
        use std::sync::atomic::AtomicUsize;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let total = PIECE as usize * 2 + 320_000;
        let data: Arc<Vec<u8>> = Arc::new((0..total).map(|i| (i % 251) as u8).collect());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let served = Arc::new(AtomicUsize::new(0));
        let server = {
            let (data, served) = (data.clone(), served.clone());
            tokio::spawn(async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else { return };
                    let (data, served) = (data.clone(), served.clone());
                    tokio::spawn(async move {
                        let mut head = Vec::new();
                        let mut buf = [0u8; 4096];
                        loop {
                            let n = socket.read(&mut buf).await.unwrap_or(0);
                            if n == 0 {
                                return;
                            }
                            head.extend_from_slice(&buf[..n]);
                            if head.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                        let text = String::from_utf8_lossy(&head).to_ascii_lowercase();
                        let range = text.lines().find_map(|line| {
                            let v = line.trim().strip_prefix("range: bytes=")?;
                            let (a, b) = v.split_once('-')?;
                            Some((a.trim().parse::<u64>().ok()?, b.trim().parse::<u64>().ok()?))
                        });
                        let end = data.len() as u64 - 1;
                        let (start, last) = range.map_or((0, end), |(a, b)| (a, b.min(end)));
                        served.fetch_add(1, Ordering::SeqCst);
                        let body = &data[start as usize..=last as usize];
                        let reply = format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{last}/{}\r\nConnection: close\r\n\r\n",
                            body.len(),
                            data.len()
                        );
                        let _ = socket.write_all(reply.as_bytes()).await;
                        let _ = socket.write_all(body).await;
                    });
                }
            })
        };

        let dir = std::env::temp_dir().join(format!("meowtify-fetch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("song.m4a.part");
        let size = download_ranged(&format!("http://{addr}/song"), &[], &part, Some(data.len() as u64), Arc::new(|_| {}))
            .await
            .unwrap();
        assert_eq!(size, data.len() as u64);
        assert_eq!(std::fs::read(&part).unwrap(), *data, "the pieces did not land in order");
        assert!(served.load(Ordering::SeqCst) >= 3, "the song was not split into ranges");
        let _ = std::fs::remove_dir_all(&dir);
        server.abort();
    }
}
