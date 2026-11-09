//! Writing audio to disk in small pieces.
//!
//! Nothing in here holds more than a 64 KiB read buffer. A download writes a `.part`
//! file and renames it only when the whole thing arrived, so a cancelled or failed
//! job never leaves a half song in the library.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::native::http;
use crate::{ProviderError, ProgressFn, Result};

/// Bytes per range request. YouTube throttles or refuses very large single requests;
/// yt-dlp uses 10 MiB, and 9 MiB keeps each request under a minute on a slow link.
pub(crate) const CHUNK: u64 = 9 * 1024 * 1024;

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

/// Downloads `url` with byte ranges, appending to `part`. Returns the file size.
///
/// `total_hint` is the size the API stated; the server's `Content-Range` replaces it.
/// Progress is a percentage of the real length, and stays under 100 until the rename.
pub(crate) async fn download_ranged(
    url: &str,
    headers: &[(&str, &str)],
    part: &Path,
    total_hint: Option<u64>,
    progress: ProgressFn,
) -> Result<u64> {
    let mut file = tokio::fs::File::create(part).await.map_err(|e| ProviderError::Io(e.to_string()))?;
    let mut offset = 0u64;
    let mut total = total_hint.filter(|t| *t > 0);
    let mut buf = vec![0u8; 64 * 1024];

    loop {
        if total.is_some_and(|t| offset >= t) {
            break;
        }
        let want = chunk_len(offset, total);
        let mut reply = http::get_range(url, offset, want, headers).await?;
        let whole = match reply.status {
            206 => false,
            200 if offset == 0 => true,
            416 if offset > 0 => break,
            status => return Err(ProviderError::Tool(format!("the audio server answered {status}"))),
        };
        if let Some(range) = reply.header("content-range").and_then(parse_content_range) {
            if range.0 != offset {
                return Err(ProviderError::Tool(format!("the server sent bytes from {}, not {offset}", range.0)));
            }
            if let Some(t) = range.2 {
                total = Some(t);
            }
        }

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
            offset += n as u64;
            if let Some(t) = total {
                if t > 0 {
                    progress((offset as f64 / t as f64 * 100.0).min(99.0) as f32);
                }
            }
            if !whole && got >= want {
                break;
            }
        }
        file.flush().await.map_err(|e| ProviderError::Io(e.to_string()))?;
        if got == 0 {
            break;
        }
        if whole {
            break;
        }
        // A short answer means the file ended, even when the server never said its size.
        if total.is_none() && got < want {
            break;
        }
    }

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
}
