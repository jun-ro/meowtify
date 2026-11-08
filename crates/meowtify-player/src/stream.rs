//! A reader for a file that a download is still writing. Reads wait for new bytes
//! instead of reporting the end of file, so the decoder can start on the first part.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use meowtify_core::growth::Growth;

/// How long a read waits for the download before it gives up.
const STALL: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(10);

/// A time after which reads stop waiting. Set while a decoder is built, when the audio worker
/// is blocked on this reader and cannot answer anything else; cleared once playback runs.
pub type GiveUp = Arc<Mutex<Option<Instant>>>;

pub struct GrowingReader {
    file: File,
    growth: Arc<Growth>,
    pos: u64,
    give_up: GiveUp,
}

impl GrowingReader {
    pub fn new(file: File, growth: Arc<Growth>) -> GrowingReader {
        GrowingReader { file, growth, pos: 0, give_up: GiveUp::default() }
    }

    /// The handle that bounds how long reads may wait. Set it before the decoder is built and
    /// clear it after; the reader itself moves into the decoder.
    pub fn give_up(&self) -> GiveUp {
        self.give_up.clone()
    }

    /// Waits until byte `want` is on disk or the download is over. Returns the
    /// number of bytes on disk then.
    fn wait_for(&self, want: u64) -> io::Result<u64> {
        let mut seen = self.growth.written();
        let mut last_change = Instant::now();
        while seen <= want && !self.growth.is_over() {
            std::thread::sleep(POLL);
            let now = self.growth.written();
            if now != seen {
                seen = now;
                last_change = Instant::now();
            } else if last_change.elapsed() > STALL {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "download stalled"));
            }
            if self.give_up.lock().unwrap().is_some_and(|t| Instant::now() >= t) {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "the start of the download is not here yet"));
            }
        }
        if self.growth.is_failed() && seen <= want {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "download failed"));
        }
        Ok(self.growth.written())
    }
}

impl Read for GrowingReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let avail = self.wait_for(self.pos)?;
        if self.pos >= avail {
            return Ok(0); // the download finished and this is the end
        }
        let take = ((avail - self.pos) as usize).min(buf.len());
        let n = self.file.read(&mut buf[..take])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for GrowingReader {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let target = match to {
            SeekFrom::Start(n) => n,
            SeekFrom::Current(d) => self.pos.checked_add_signed(d).ok_or_else(bad_seek)?,
            SeekFrom::End(d) => {
                // The end is known only when the file is complete.
                let end = self.wait_for(u64::MAX - 1)?;
                if !self.growth.is_done() {
                    return Err(bad_seek());
                }
                end.checked_add_signed(d).ok_or_else(bad_seek)?
            }
        };
        if target > self.pos {
            // Do not move past what is on disk; wait for it.
            self.wait_for(target.saturating_sub(1))?;
        }
        self.pos = self.file.seek(SeekFrom::Start(target))?;
        Ok(self.pos)
    }
}

fn bad_seek() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "invalid seek")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("meowtify-stream-{}-{name}", std::process::id()))
    }

    #[test]
    fn reads_while_the_file_grows() {
        let path = temp("grow");
        std::fs::write(&path, b"").unwrap();
        let growth = Growth::new();
        let mut reader = GrowingReader::new(File::open(&path).unwrap(), growth.clone());

        let writer = {
            let (path, growth) = (path.clone(), growth.clone());
            std::thread::spawn(move || {
                let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
                for chunk in [&b"abcd"[..], b"efgh", b"ij"] {
                    std::thread::sleep(Duration::from_millis(40));
                    f.write_all(chunk).unwrap();
                    f.flush().unwrap();
                    growth.set_written(growth.written() + chunk.len() as u64);
                }
                growth.finish(10);
            })
        };

        let mut all = Vec::new();
        reader.read_to_end(&mut all).unwrap();
        writer.join().unwrap();
        assert_eq!(all, b"abcdefghij");

        // A finished file is seekable, also from the end.
        assert_eq!(reader.seek(SeekFrom::Start(2)).unwrap(), 2);
        let mut two = [0u8; 2];
        reader.read_exact(&mut two).unwrap();
        assert_eq!(&two, b"cd");
        assert_eq!(reader.seek(SeekFrom::End(-1)).unwrap(), 9);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_read_gives_up_at_its_deadline() {
        let path = temp("giveup");
        std::fs::write(&path, b"").unwrap();
        let mut reader = GrowingReader::new(File::open(&path).unwrap(), Growth::new());
        *reader.give_up().lock().unwrap() = Some(Instant::now() + Duration::from_millis(50));
        let t = Instant::now();
        let err = reader.read(&mut [0u8; 4]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(t.elapsed() < Duration::from_secs(5));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_failed_download_is_an_error_not_an_end() {
        let path = temp("fail");
        std::fs::write(&path, b"xy").unwrap();
        let growth = Growth::new();
        growth.set_written(2);
        let mut reader = GrowingReader::new(File::open(&path).unwrap(), growth.clone());
        let mut two = [0u8; 2];
        reader.read_exact(&mut two).unwrap();
        growth.fail();
        let mut more = [0u8; 1];
        assert!(reader.read(&mut more).is_err());
        let _ = std::fs::remove_file(path);
    }

    /// Needs a real fragmented m4a, as YouTube serves it:
    /// `MEOWTIFY_TEST_M4A=path cargo test -p meowtify-player decodes_from_a_growing -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn decodes_from_a_growing_unseekable_file() {
        let src = std::env::var("MEOWTIFY_TEST_M4A").expect("set MEOWTIFY_TEST_M4A");
        let bytes = std::fs::read(&src).unwrap();
        let total = bytes.len() as u64;

        // Reference: the whole file, seekable.
        let whole = rodio::Decoder::try_from(File::open(&src).unwrap()).unwrap();
        let expected = whole.count();

        let path = temp("decode");
        std::fs::write(&path, b"").unwrap();
        let growth = Growth::new();
        let writer = {
            let (path, growth) = (path.clone(), growth.clone());
            std::thread::spawn(move || {
                let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
                for chunk in bytes.chunks(16 * 1024) {
                    f.write_all(chunk).unwrap();
                    f.flush().unwrap();
                    growth.set_written(growth.written() + chunk.len() as u64);
                    std::thread::sleep(Duration::from_millis(2));
                }
                growth.finish(total);
            })
        };
        let t = Instant::now();
        let decoder = rodio::Decoder::builder()
            .with_data(GrowingReader::new(File::open(&path).unwrap(), growth.clone()))
            .with_hint("m4a")
            .with_seekable(false)
            .build()
            .expect("probe a growing file");
        let first_sample_after = t.elapsed();
        let written_then = growth.written();
        let got = decoder.count();
        writer.join().unwrap();
        eprintln!("probe done after {first_sample_after:?} with {written_then} of {total} bytes; samples {got} vs {expected}");
        assert!(written_then < total, "the decoder waited for the whole file");
        assert_eq!(got, expected);
        let _ = std::fs::remove_file(path);
    }
}
