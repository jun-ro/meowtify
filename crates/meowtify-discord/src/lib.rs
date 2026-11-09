//! Discord Rich Presence: shows "Listening to <song>" on the user's profile.
//!
//! Discord's desktop app listens on a local pipe. No network is used and no
//! account is needed. A worker thread owns the pipe, so a slow or absent Discord
//! never blocks the UI or the core.
//!
//! - `Presence::start` follows the core's events and turns them into activities.
//! - Nothing is sent while the feature is off, and the activity is cleared when it
//!   is switched off, on pause and on stop.
//! - Discord limits updates, so writes are at least `MIN_GAP` apart. The newest
//!   state wins; the ones in between are dropped.
//! - Every exchange with Discord has a deadline. A late one is cancelled at the pipe, so its
//!   thread ends, and at most one such thread exists at a time.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use meowtify_core::{Event, PlaybackState};
use serde_json::{json, Value};
use tokio::sync::broadcast;

/// The Discord application that names the activity ("Listening to Meowtify").
pub const CLIENT_ID: &str = "1568474724776804552";
/// Name of the uploaded image asset used when a song has no cover URL.
const FALLBACK_IMAGE: &str = "meowtify";
/// Least time between two writes to Discord.
const MIN_GAP: Duration = Duration::from_secs(5);
/// Wait before trying again when Discord is not running.
const RETRY: Duration = Duration::from_secs(15);
/// A shift in the start time up to this is jitter. More is a seek.
const SEEK_SLACK_S: i64 = 3;

/// What the profile should show.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Activity {
    title: String,
    artist: String,
    cover: Option<String>,
    /// Unix seconds at which the song started, as if it played from the beginning.
    start: i64,
    /// Unix seconds at which it ends. 0 when the length is unknown.
    end: i64,
}

impl Activity {
    /// Same song and same place in it, within the slack for timing jitter.
    fn same(&self, o: &Activity) -> bool {
        self.title == o.title
            && self.artist == o.artist
            && self.cover == o.cover
            && (self.start - o.start).abs() <= SEEK_SLACK_S
    }

    fn from_state(s: &PlaybackState) -> Option<Activity> {
        if !s.is_playing {
            return None;
        }
        let t = s.track.as_ref()?;
        let now = unix_secs();
        let start = now - i64::from(s.progress_ms / 1000);
        let end = if t.duration_ms > 0 { start + i64::from(t.duration_ms / 1000) } else { 0 };
        let artist = t.artists.iter().map(|a| a.name.as_ref()).collect::<Vec<_>>().join(", ");
        Some(Activity { title: t.name.to_string(), artist, cover: cover_url(&t.uri), start, end })
    }

    fn payload(&self) -> Value {
        let mut timestamps = json!({ "start": self.start });
        if self.end > 0 {
            timestamps["end"] = json!(self.end);
        }
        let mut a = json!({
            "type": 2, // Listening
            "details": clean(&self.title),
            "timestamps": timestamps,
            "assets": {
                "large_image": self.cover.as_deref().unwrap_or(FALLBACK_IMAGE),
                "large_text": clean(&self.title),
                "small_image": FALLBACK_IMAGE,
                "small_text": "Meowtify",
            },
        });
        if !self.artist.is_empty() {
            a["state"] = json!(clean(&self.artist));
        }
        a
    }
}

fn unix_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Discord wants 2 to 128 characters in a text field.
fn clean(s: &str) -> String {
    let mut out: String = s.chars().take(128).collect();
    if out.chars().count() < 2 {
        out.push('\u{2800}'); // a blank braille cell
    }
    out
}

/// The cover address of a download, when its file name carries a YouTube id.
fn cover_url(path: &str) -> Option<String> {
    meowtify_core::cover_id(path).map(|id| meowtify_core::youtube_thumbnail(&id))
}

enum Msg {
    Show(Option<Activity>),
    Enabled(bool),
}

/// Handle to the presence worker. Cheap to keep; dropping it stops the worker.
pub struct Presence {
    tx: mpsc::Sender<Msg>,
}

impl Presence {
    /// Starts following `events`. Nothing is sent to Discord until `enabled` is true.
    pub fn start(rt: &tokio::runtime::Handle, mut events: broadcast::Receiver<Event>, enabled: bool) -> Presence {
        let (tx, rx) = mpsc::channel();
        let _ = tx.send(Msg::Enabled(enabled));
        std::thread::Builder::new()
            .name("discord-presence".into())
            .spawn(move || worker(rx))
            .expect("spawn discord thread");
        let events_tx = tx.clone();
        rt.spawn(async move {
            let mut last: Option<Activity> = None;
            loop {
                let ev = match events.recv().await {
                    Ok(e) => e,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return,
                };
                let Event::PlaybackChanged(state) = ev else { continue };
                let next = state.as_ref().and_then(Activity::from_state);
                let unchanged = match (&last, &next) {
                    (None, None) => true,
                    (Some(a), Some(b)) => a.same(b),
                    _ => false,
                };
                if unchanged {
                    continue;
                }
                last = next.clone();
                if events_tx.send(Msg::Show(next)).is_err() {
                    return;
                }
            }
        });
        Presence { tx }
    }

    pub fn set_enabled(&self, on: bool) {
        let _ = self.tx.send(Msg::Enabled(on));
    }
}

struct Worker {
    enabled: bool,
    /// What the profile should show now.
    want: Option<Activity>,
    /// What Discord was last told.
    shown: Option<Activity>,
    conn: Option<Conn>,
    last_write: Option<Instant>,
    retry_at: Option<Instant>,
}

fn worker(rx: mpsc::Receiver<Msg>) {
    let mut w = Worker { enabled: false, want: None, shown: None, conn: None, last_write: None, retry_at: None };
    loop {
        let wait = w.wait();
        match rx.recv_timeout(wait) {
            Ok(m) => w.apply(m),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                w.enabled = false;
                w.sync(true);
                return;
            }
        }
        while let Ok(m) = rx.try_recv() {
            w.apply(m);
        }
        w.sync(false);
    }
}

impl Worker {
    fn apply(&mut self, m: Msg) {
        match m {
            Msg::Show(a) => self.want = a,
            Msg::Enabled(on) => self.enabled = on,
        }
    }

    /// What Discord should show: nothing while the feature is off.
    fn target(&self) -> Option<&Activity> {
        if self.enabled {
            self.want.as_ref()
        } else {
            None
        }
    }

    fn dirty(&self) -> bool {
        match (self.target(), &self.shown) {
            (None, None) => false,
            (Some(a), Some(b)) => !a.same(b),
            _ => true,
        }
    }

    /// How long to sleep before there is something to do.
    fn wait(&self) -> Duration {
        if !self.dirty() {
            let now = Instant::now();
            return match (&self.conn, self.last_write) {
                (Some(_), Some(t)) if self.shown.is_some() => (t + LIVENESS).saturating_duration_since(now).max(Duration::from_millis(50)),
                _ => Duration::from_secs(60),
            };
        }
        let now = Instant::now();
        let gap = self.last_write.map_or(now, |t| t + MIN_GAP);
        let at = self.retry_at.map_or(gap, |r| r.max(gap));
        at.saturating_duration_since(now).max(Duration::from_millis(50))
    }

    /// Writes the target to Discord if it differs and the gap has passed.
    /// `force` skips the gap, for the last write at exit.
    /// The connection has been quiet for a while while something is shown.
    fn liveness_due(&self) -> bool {
        self.conn.is_some() && self.shown.is_some() && self.target().is_some() && self.last_write.is_some_and(|t| t.elapsed() >= LIVENESS)
    }

    fn sync(&mut self, force: bool) {
        if !self.dirty() && !self.liveness_due() {
            if !self.enabled && self.shown.is_none() {
                self.conn = None; // switched off and cleared: let go of the pipe
            }
            return;
        }
        let now = Instant::now();
        if !force
            && (self.last_write.is_some_and(|t| now < t + MIN_GAP) || self.retry_at.is_some_and(|r| now < r))
        {
            return;
        }
        let target = self.target().cloned();
        if self.conn.is_none() {
            match Conn::open() {
                Some(c) => self.conn = Some(c),
                None => {
                    // Discord is not running. There is nothing to clear either.
                    if target.is_none() {
                        self.shown = None;
                    } else {
                        self.retry_at = Some(now + RETRY);
                    }
                    return;
                }
            }
        }
        let activity = target.as_ref().map_or(Value::Null, Activity::payload);
        // The exchange runs under a deadline: a Discord that does not answer cannot freeze
        // this worker. The connection is dropped when it fails or is late.
        let ok = match self.conn.take().and_then(|c| with_deadline(c, move |c| c.set_activity(activity))) {
            Some((c, Ok(()))) => {
                self.conn = Some(c);
                true
            }
            _ => false,
        };
        self.last_write = Some(now);
        if ok {
            self.shown = target;
            self.retry_at = None;
        } else {
            self.conn = None; // Discord closed or restarted. Reconnect next time.
            self.retry_at = Some(now + RETRY);
        }
    }
}

const OP_HANDSHAKE: u32 = 0;
const OP_FRAME: u32 = 1;
const OP_CLOSE: u32 = 2;
const OP_PING: u32 = 3;
const OP_PONG: u32 = 4;
/// How long one exchange with Discord may take before the connection is given up.
const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// While connected and showing something, the activity is sent again this often. A Discord
/// that restarted is noticed by the failed write.
const LIVENESS: Duration = Duration::from_secs(60);
/// Frames read while waiting for the answer to one request.
const MAX_FRAMES: usize = 16;

struct Conn {
    pipe: Pipe,
    nonce: u64,
}

/// An open pipe or socket. Clones share it, so one thread can cancel what another is blocked
/// on; the handle stays open until the last clone is dropped.
#[derive(Clone)]
struct Pipe(Arc<Raw>);

#[cfg(windows)]
type Raw = std::fs::File;
#[cfg(unix)]
type Raw = std::os::unix::net::UnixStream;
#[cfg(not(any(windows, unix)))]
struct Raw;
#[cfg(not(any(windows, unix)))]
impl Read for &Raw {
    fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::ErrorKind::Unsupported.into())
    }
}
#[cfg(not(any(windows, unix)))]
impl Write for &Raw {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::ErrorKind::Unsupported.into())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Read for Pipe {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        (&*self.0).read(buf)
    }
}

impl Write for Pipe {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        (&*self.0).write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        (&*self.0).flush()
    }
}

impl Pipe {
    /// Makes a read or write that is blocked on this pipe, in any thread, fail now.
    fn cancel(&self) {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            #[link(name = "kernel32")]
            extern "system" {
                fn CancelIoEx(file: *mut std::ffi::c_void, overlapped: *mut std::ffi::c_void) -> i32;
            }
            // SAFETY: the handle is open (this `Pipe` keeps it so) and a null overlapped
            // pointer asks to cancel every pending request on it. It changes no memory.
            unsafe {
                CancelIoEx(self.0.as_raw_handle(), std::ptr::null_mut());
            }
        }
        #[cfg(unix)]
        {
            let _ = self.0.shutdown(std::net::Shutdown::Both);
        }
    }
}

impl Conn {
    /// Connects to the first Discord pipe that answers and completes the handshake.
    fn open() -> Option<Conn> {
        for n in 0..10 {
            let Some(pipe) = connect(n) else { continue };
            let c = Conn { pipe, nonce: 0 };
            let hello = json!({ "v": 1, "client_id": CLIENT_ID });
            let done = with_deadline(c, move |c| {
                c.send(OP_HANDSHAKE, &hello)?;
                c.read_reply(None).map(|_| ())
            });
            if let Some((c, Ok(()))) = done {
                return Some(c);
            }
        }
        None
    }

    fn set_activity(&mut self, activity: Value) -> std::io::Result<()> {
        self.nonce += 1;
        let msg = json!({
            "cmd": "SET_ACTIVITY",
            "args": { "pid": std::process::id(), "activity": activity },
            "nonce": self.nonce.to_string(),
        });
        self.send(OP_FRAME, &msg)?;
        let reply = self.read_reply(Some(&self.nonce.to_string()))?;
        if reply["evt"] == "ERROR" {
            return Err(std::io::Error::other(format!("Discord refused the activity: {}", reply["data"]["message"])));
        }
        Ok(())
    }

    fn send(&mut self, op: u32, body: &Value) -> std::io::Result<()> {
        let data = serde_json::to_vec(body).map_err(std::io::Error::other)?;
        let mut frame = Vec::with_capacity(8 + data.len());
        frame.extend_from_slice(&op.to_le_bytes());
        frame.extend_from_slice(&(data.len() as u32).to_le_bytes());
        frame.extend_from_slice(&data);
        self.pipe.write_all(&frame)?;
        self.pipe.flush()
    }

    /// Reads frames until the reply to the request with `nonce` (or, for the handshake, the
    /// first data frame). Pings are answered; a close frame is an error. Discord answers
    /// every write, and an unread reply would eventually fill the pipe.
    fn read_reply(&mut self, nonce: Option<&str>) -> std::io::Result<Value> {
        for _ in 0..MAX_FRAMES {
            let mut head = [0u8; 8];
            self.pipe.read_exact(&mut head)?;
            let op = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
            let len = u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as usize;
            if len > 64 * 1024 {
                return Err(std::io::Error::other("reply too large"));
            }
            let mut buf = vec![0u8; len];
            self.pipe.read_exact(&mut buf)?;
            match op {
                OP_PING => {
                    let mut frame = Vec::with_capacity(8 + len);
                    frame.extend_from_slice(&OP_PONG.to_le_bytes());
                    frame.extend_from_slice(&(len as u32).to_le_bytes());
                    frame.extend_from_slice(&buf);
                    self.pipe.write_all(&frame)?;
                    self.pipe.flush()?;
                }
                OP_CLOSE => return Err(std::io::Error::other("Discord closed the connection")),
                _ => {
                    let value: Value = serde_json::from_slice(&buf).map_err(std::io::Error::other)?;
                    match nonce {
                        // Events that belong to nothing we asked are skipped.
                        Some(n) if value["nonce"].as_str() != Some(n) => continue,
                        _ => return Ok(value),
                    }
                }
            }
        }
        Err(std::io::Error::other("no reply from Discord"))
    }
}

/// How long a cancelled exchange gets to give up its thread.
const REAP: Duration = Duration::from_secs(1);
/// Exchange threads that have not ended yet.
static LIVE: AtomicUsize = AtomicUsize::new(0);

/// Runs `f` on the connection under the deadline [`IO_TIMEOUT`]. Returns the connection with
/// the result, or `None` when it was too slow, when the thread could not start, or when an
/// earlier exchange is still stuck.
fn with_deadline<T: Send + 'static>(
    conn: Conn,
    f: impl FnOnce(&mut Conn) -> std::io::Result<T> + Send + 'static,
) -> Option<(Conn, std::io::Result<T>)> {
    let pipe = conn.pipe.clone();
    run_with_deadline(&LIVE, conn, move || pipe.cancel(), IO_TIMEOUT, REAP, f)
}

/// The deadline logic, apart from Discord so it can be tested.
///
/// `f` runs in a helper thread. If it is not done after `timeout`, `cancel` is called to break
/// whatever it is blocked on, and the thread gets `reap` to finish. `live` counts the helper
/// threads that exist; while one is still stuck, no other starts, so a Discord that never
/// answers cannot pile up threads.
fn run_with_deadline<C: Send + 'static, T: Send + 'static>(
    live: &'static AtomicUsize,
    mut conn: C,
    cancel: impl FnOnce(),
    timeout: Duration,
    reap: Duration,
    f: impl FnOnce(&mut C) -> std::io::Result<T> + Send + 'static,
) -> Option<(C, std::io::Result<T>)> {
    if live.load(Ordering::SeqCst) > 0 {
        return None;
    }
    live.fetch_add(1, Ordering::SeqCst);
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new().name("discord-io".into()).spawn(move || {
        let r = f(&mut conn);
        live.fetch_sub(1, Ordering::SeqCst);
        let _ = tx.send((conn, r));
    });
    if spawned.is_err() {
        live.fetch_sub(1, Ordering::SeqCst);
        return None;
    }
    match rx.recv_timeout(timeout) {
        Ok(done) => Some(done),
        Err(_) => {
            cancel();
            // The cancelled exchange fails and its thread ends. Give it a moment, and drop what
            // it returns: a connection that was late is not trusted.
            let _ = rx.recv_timeout(reap);
            None
        }
    }
}

#[cfg(windows)]
fn connect(n: u32) -> Option<Pipe> {
    let f = std::fs::OpenOptions::new().read(true).write(true).open(format!(r"\\.\pipe\discord-ipc-{n}")).ok()?;
    Some(Pipe(Arc::new(f)))
}

#[cfg(unix)]
fn connect(n: u32) -> Option<Pipe> {
    let dirs = ["XDG_RUNTIME_DIR", "TMPDIR", "TMP", "TEMP"].iter().filter_map(|k| std::env::var(k).ok());
    for dir in dirs.chain(std::iter::once("/tmp".to_string())) {
        if let Ok(s) = std::os::unix::net::UnixStream::connect(format!("{dir}/discord-ipc-{n}")) {
            // The deadline thread covers a stall too, but a timeout ends the call itself.
            let _ = s.set_read_timeout(Some(IO_TIMEOUT));
            let _ = s.set_write_timeout(Some(IO_TIMEOUT));
            return Some(Pipe(Arc::new(s)));
        }
    }
    None
}

#[cfg(not(any(windows, unix)))]
fn connect(_n: u32) -> Option<Pipe> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn act(start: i64) -> Activity {
        Activity { title: "Song".into(), artist: "Band".into(), cover: None, start, end: start + 200 }
    }

    #[test]
    fn jitter_is_the_same_but_a_seek_is_not() {
        assert!(act(1000).same(&act(1002)));
        assert!(!act(1000).same(&act(1030)));
    }

    #[test]
    fn text_is_kept_within_discords_limits() {
        assert_eq!(clean("x").chars().count(), 2);
        assert_eq!(clean(&"a".repeat(300)).chars().count(), 128);
        assert_eq!(clean("ok"), "ok");
    }

    #[test]
    fn cover_comes_from_the_id_in_the_file_name() {
        assert_eq!(
            cover_url(r"C:\Music\laced up [NWrZ__uGAf4].m4a").as_deref(),
            Some("https://i.ytimg.com/vi/NWrZ__uGAf4/mqdefault.jpg")
        );
        assert_eq!(cover_url(r"C:\Music\plain.mp3"), None);
    }

    #[test]
    fn payload_is_a_listening_activity_with_a_progress_bar() {
        let p = act(1000).payload();
        assert_eq!(p["type"], 2);
        assert_eq!(p["details"], "Song");
        assert_eq!(p["state"], "Band");
        assert_eq!(p["timestamps"]["start"], 1000);
        assert_eq!(p["timestamps"]["end"], 1200);
        assert_eq!(p["assets"]["large_image"], FALLBACK_IMAGE);
        let mut none = act(1000);
        none.artist.clear();
        assert!(none.payload().get("state").is_none());
    }

    #[test]
    fn nothing_is_sent_while_off_and_a_clear_follows_switching_off() {
        let mut w =
            Worker { enabled: false, want: Some(act(1000)), shown: None, conn: None, last_write: None, retry_at: None };
        assert!(!w.dirty(), "off: nothing to show, nothing shown");
        w.enabled = true;
        assert!(w.dirty());
        w.shown = Some(act(1000));
        assert!(!w.dirty());
        w.enabled = false;
        assert!(w.dirty(), "must clear what Discord still shows");
    }

    /// A pipe stand-in that blocks reads until it is cancelled.
    fn blocking_pipe() -> (mpsc::Receiver<()>, impl FnOnce()) {
        let (tx, rx) = mpsc::channel();
        (rx, move || {
            let _ = tx.send(());
        })
    }

    #[test]
    fn a_late_exchange_is_cancelled_and_its_thread_ends() {
        static LIVE: AtomicUsize = AtomicUsize::new(0);
        let (blocked_on, cancel) = blocking_pipe();
        let out = run_with_deadline(&LIVE, (), cancel, Duration::from_millis(50), Duration::from_secs(5), move |_| {
            blocked_on.recv().map_err(|_| std::io::Error::other("cancelled")).map(|_| ())
        });
        assert!(out.is_none(), "too slow: no result");
        assert_eq!(LIVE.load(Ordering::SeqCst), 0, "cancelling ended the helper thread");
        let ok = run_with_deadline(&LIVE, (), || {}, Duration::from_secs(5), Duration::ZERO, |_| Ok(7));
        assert_eq!(ok.map(|(_, r)| r.unwrap()), Some(7), "the next exchange runs");
    }

    #[test]
    fn a_thread_that_cannot_be_cancelled_is_never_joined_by_a_second() {
        static LIVE: AtomicUsize = AtomicUsize::new(0);
        let (release, stuck) = mpsc::channel::<()>();
        let out = run_with_deadline(&LIVE, (), || {}, Duration::from_millis(30), Duration::from_millis(30), move |_| {
            let _ = stuck.recv(); // ignores the cancel
            Ok(())
        });
        assert!(out.is_none());
        assert_eq!(LIVE.load(Ordering::SeqCst), 1, "the stuck thread is counted");
        let second = run_with_deadline(&LIVE, (), || {}, Duration::from_secs(1), Duration::ZERO, |_| {
            Ok(())
        });
        assert!(second.is_none(), "no second helper while one is stuck");
        assert_eq!(LIVE.load(Ordering::SeqCst), 1, "so the count cannot grow");
        drop(release); // lets the stuck thread finish
        for _ in 0..100 {
            if LIVE.load(Ordering::SeqCst) == 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(LIVE.load(Ordering::SeqCst), 0);
    }

    /// The real thing on Windows: a read blocked on a pipe, ended by `Pipe::cancel` from
    /// another thread. A child's output pipe behaves like Discord's: a synchronous handle.
    #[cfg(windows)]
    #[test]
    fn cancelling_ends_a_read_blocked_on_a_real_pipe() {
        use std::os::windows::io::OwnedHandle;
        use std::process::{Command, Stdio};
        let mut child = Command::new("ping").args(["-n", "30", "127.0.0.1"]).stdout(Stdio::piped()).spawn().unwrap();
        let out = child.stdout.take().unwrap();
        // ping prints its header at once; wait for it so the next read has nothing to return.
        let mut pipe = Pipe(Arc::new(std::fs::File::from(OwnedHandle::from(out))));
        let mut buf = [0u8; 4096];
        let _ = pipe.read(&mut buf);
        let reader = pipe.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut buf = [0u8; 4096];
            // Reads until the pipe fails; the first replies from ping are short.
            let r = loop {
                match reader.read(&mut buf) {
                    Ok(0) => break Ok(0),
                    Ok(_) => continue,
                    Err(e) => break Err(e),
                }
            };
            let _ = tx.send(r.is_err());
        });
        std::thread::sleep(Duration::from_millis(1500));
        pipe.cancel();
        let failed = rx.recv_timeout(Duration::from_secs(5)).expect("the blocked read ended");
        assert!(failed, "the read failed instead of completing");
        let _ = child.kill();
    }

    /// Needs Discord running. Run with `cargo test -p meowtify-discord -- --ignored`.
    #[test]
    #[ignore]
    fn live_discord_accepts_and_clears_an_activity() {
        let mut c = Conn::open().expect("Discord is not running");
        let now = unix_secs();
        let mut a = act(now - 30);
        a.cover = cover_url("x [NWrZ__uGAf4].m4a");
        c.set_activity(a.payload()).unwrap();
        std::thread::sleep(Duration::from_secs(8));
        c.set_activity(Value::Null).unwrap();
    }
}
