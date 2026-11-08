//! Local playback. One audio thread owns the output device and the queue.
//! [`LocalPlayer`] talks to it over a channel and implements [`PlaybackEngine`],
//! so the core actor and the UI treat it like any other engine.

use std::fs::File;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use meowtify_core::growth::{self, Growth};
use meowtify_core::{Error, PlaybackEngine, PlaybackState, Place, Queue, Repeat, Result, Selection, Track};
use meowtify_library::Library;
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player};
use tokio::sync::oneshot;

mod stream;
use stream::GrowingReader;

/// How often the audio thread updates the position and checks for track end.
const TICK: Duration = Duration::from_millis(200);
/// Previous restarts the track when it has played longer than this.
const RESTART_AFTER: Duration = Duration::from_secs(3);
/// Upcoming tracks reported by [`PlaybackEngine::queue`].
const UPCOMING: usize = 50;
/// A queue longer than this is trimmed behind the playing song.
const MAX_QUEUE: usize = 1000;
/// Songs kept behind the playing one when the queue is trimmed, for Previous.
const KEEP_BEHIND: usize = 100;
/// Unplayable files stepped over in one move to the next song.
const MAX_SKIPS: usize = 5;
/// The loudness of a volume setting. A straight line from 0 to 1 is wrong for ears: the
/// slider spends its whole lower half on sounds that are nearly the same loudness. The
/// cube makes 50% about 18 dB down and 10% nearly silent (-60 dB).
fn gain(percent: u8) -> f32 {
    let x = f32::from(percent.min(100)) / 100.0;
    x * x * x
}

/// How long a caller waits for the audio thread. A command that waited longer than this
/// in the channel is dropped unexecuted, so a failed command never takes effect later.
const ASK_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest the worker waits for the start of a downloading file while it builds a decoder.
/// Shorter than `ASK_TIMEOUT`, so the caller still gets the error.
const BUILD_WAIT: Duration = Duration::from_secs(7);

type Reply = oneshot::Sender<Result<()>>;

/// A command with the time after which nobody waits for its answer.
struct Request {
    msg: Msg,
    deadline: Instant,
}

enum Msg {
    PlayTrack { queue: Arc<[Track]>, index: usize, reply: Reply },
    Resume(Reply),
    Pause(Reply),
    Next(Reply),
    Previous(Reply),
    Seek(u32, Reply),
    Volume(u8),
    Shuffle(bool, Reply),
    Repeat(Repeat, Reply),
    /// Puts a track in the queue: right after the loaded one (`next`) or after
    /// the tracks already queued by hand.
    Enqueue { track: Track, place: Place, reply: Reply },
    /// Plays a song that waits in the queue, now.
    Jump { uri: Arc<str>, reply: Reply },
    Shutdown,
}

impl Msg {
    fn reply(&self) -> Option<&Reply> {
        match self {
            Msg::PlayTrack { reply, .. }
            | Msg::Enqueue { reply, .. }
            | Msg::Resume(reply)
            | Msg::Pause(reply)
            | Msg::Next(reply)
            | Msg::Previous(reply)
            | Msg::Seek(_, reply)
            | Msg::Shuffle(_, reply)
            | Msg::Repeat(_, reply)
            | Msg::Jump { reply, .. } => Some(reply),
            Msg::Volume(_) | Msg::Shutdown => None,
        }
    }

    /// True when the caller has gone away.
    fn abandoned(&self) -> bool {
        self.reply().is_some_and(|r| r.is_closed())
    }

    /// Answers a command that was not run because its time was over.
    /// Whether running this command can change the loaded song or the list of upcoming ones.
    /// Pausing, seeking, the volume and the repeat mode cannot.
    fn changes_queue(&self) -> bool {
        !matches!(self, Msg::Pause(_) | Msg::Seek(..) | Msg::Volume(_) | Msg::Repeat(..) | Msg::Shutdown)
    }

    fn expire(self) {
        let reply = match self {
            Msg::PlayTrack { reply, .. }
            | Msg::Enqueue { reply, .. }
            | Msg::Resume(reply)
            | Msg::Pause(reply)
            | Msg::Next(reply)
            | Msg::Previous(reply)
            | Msg::Seek(_, reply)
            | Msg::Shuffle(_, reply)
            | Msg::Repeat(_, reply)
            | Msg::Jump { reply, .. } => reply,
            Msg::Volume(_) | Msg::Shutdown => return,
        };
        let _ = reply.send(Err(Error::Decode("the command waited too long".into())));
    }
}

/// What readers see. Updated by the audio thread.
#[derive(Default, Clone)]
struct Snapshot {
    current: Option<Track>,
    /// Shared with every reader of the queue; replaced, never changed in place.
    upcoming: Arc<[Track]>,
    /// Counts the times `upcoming` or the loaded track changed; see `Queue::rev`.
    queue_rev: u64,
    /// How many of `upcoming` the user queued, and how many autoplay added after those.
    manual: usize,
    auto: usize,
    playing: bool,
    progress_ms: u32,
    /// The queue ran out; see `PlaybackState::ended`.
    ended: bool,
    volume: u8,
    shuffle: bool,
    repeat: Repeat,
}

pub struct LocalPlayer {
    library: Arc<Library>,
    tx: Sender<Request>,
    snap: Arc<Mutex<Snapshot>>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl LocalPlayer {
    pub fn new(library: Arc<Library>) -> LocalPlayer {
        let (tx, rx) = mpsc::channel();
        let snap = Arc::new(Mutex::new(Snapshot { volume: 80, ..Default::default() }));
        let thread = {
            let snap = snap.clone();
            std::thread::Builder::new()
                .name("audio".into())
                .spawn(move || Worker::new(snap).run(rx))
                .expect("spawn audio thread")
        };
        LocalPlayer { library, tx, snap, thread: Mutex::new(Some(thread)) }
    }

    /// Sends a command and waits for the answer without blocking the async runtime: the
    /// downloads that feed a growing file run on the same runtime. If the caller gives up
    /// (timeout, or the future is dropped) the audio thread skips the command.
    async fn ask(&self, make: impl FnOnce(Reply) -> Msg) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        let deadline = Instant::now() + ASK_TIMEOUT;
        self.tx.send(Request { msg: make(reply), deadline }).map_err(|_| Error::Decode("audio thread stopped".into()))?;
        match tokio::time::timeout_at(deadline.into(), rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(Error::Decode("audio thread stopped".into())),
            Err(_) => Err(Error::Decode("audio thread did not answer".into())),
        }
    }

    /// Reads what is needed from the snapshot under the lock. Nothing is copied that `f` does
    /// not take.
    fn read<R>(&self, f: impl FnOnce(&Snapshot) -> R) -> R {
        f(&self.snap.lock().unwrap())
    }
}

impl Drop for LocalPlayer {
    fn drop(&mut self) {
        let _ = self.tx.send(Request { msg: Msg::Shutdown, deadline: Instant::now() });
        if let Some(t) = self.thread.lock().unwrap().take() {
            let _ = t.join();
        }
    }
}

#[async_trait]
impl PlaybackEngine for LocalPlayer {
    async fn play(&self) -> Result<()> {
        let (loaded, shuffle) = self.read(|s| (s.current.is_some(), s.shuffle));
        if loaded {
            return self.ask(Msg::Resume).await;
        }
        // Nothing loaded yet: start the library from the top, or anywhere when shuffling.
        let queue = self.library.tracks();
        if queue.is_empty() {
            return Err(Error::NotFound);
        }
        let index = if shuffle { (next_rand(&mut seed()) % queue.len() as u64) as usize } else { 0 };
        self.ask(|reply| Msg::PlayTrack { queue, index, reply }).await
    }

    async fn pause(&self) -> Result<()> {
        self.ask(Msg::Pause).await
    }

    async fn next(&self) -> Result<()> {
        self.ask(Msg::Next).await
    }

    async fn previous(&self) -> Result<()> {
        self.ask(Msg::Previous).await
    }

    async fn seek(&self, position_ms: u32) -> Result<()> {
        self.ask(|r| Msg::Seek(position_ms, r)).await
    }

    async fn set_volume(&self, percent: u8) -> Result<()> {
        self.tx.send(Request { msg: Msg::Volume(percent.min(100)), deadline: Instant::now() + ASK_TIMEOUT }).map_err(|_| Error::Decode("audio thread stopped".into()))
    }

    async fn set_shuffle(&self, on: bool) -> Result<()> {
        self.ask(|r| Msg::Shuffle(on, r)).await
    }

    async fn set_repeat(&self, mode: Repeat) -> Result<()> {
        self.ask(|r| Msg::Repeat(mode, r)).await
    }

    async fn enqueue(&self, uri: &str, place: Place) -> Result<()> {
        let track = self.library.find(uri).ok_or(Error::NotFound)?;
        self.ask(|reply| Msg::Enqueue { track, place, reply }).await
    }

    async fn jump(&self, uri: &str) -> Result<()> {
        let uri: Arc<str> = uri.into();
        self.ask(|reply| Msg::Jump { uri, reply }).await
    }

    async fn play_from(&self, selection: &Selection) -> Result<()> {
        let (queue, index): (Arc<[Track]>, usize) = match selection {
            Selection::Single(uri) => (Arc::from(vec![self.library.find(uri).ok_or(Error::NotFound)?]), 0),
            Selection::Library(uri) => {
                let queue = self.library.tracks();
                let index = queue.iter().position(|t| t.uri == *uri).ok_or(Error::NotFound)?;
                (queue, index)
            }
            Selection::Playlist { id, from } => {
                let queue: Arc<[Track]> = self.library.playlist_tracks(id).into();
                if queue.is_empty() {
                    return Err(Error::NotFound);
                }
                let index = match from {
                    Some(uri) => queue.iter().position(|t| t.uri == *uri).ok_or(Error::NotFound)?,
                    None => 0,
                };
                (queue, index)
            }
        };
        self.ask(|reply| Msg::PlayTrack { queue, index, reply }).await
    }

    async fn playback_state(&self) -> Result<Option<PlaybackState>> {
        Ok(self.read(|s| {
            s.current.clone().map(|t| PlaybackState {
                is_playing: s.playing,
                progress_ms: s.progress_ms,
                track: Some(t),
                volume: Some(s.volume),
                shuffle: s.shuffle,
                repeat: s.repeat,
                ended: s.ended,
            })
        }))
    }

    async fn queue(&self) -> Result<Queue> {
        Ok(self.read(|s| Queue { current: s.current.clone(), upcoming: s.upcoming.clone(), manual: s.manual, auto: s.auto, rev: s.queue_rev }))
    }
}

/// A track opened and ready, not yet the loaded one.
struct Prepared {
    player: Player,
    streaming: Option<Arc<Growth>>,
}

/// Lives on the audio thread. Owns the device, the queue and the current player.
struct Worker {
    snap: Arc<Mutex<Snapshot>>,
    sink: Option<MixerDeviceSink>,
    sink_error: Option<String>,
    player: Option<Player>,
    /// Set while the loaded track plays from a file that is still downloading.
    /// That decoder cannot seek; see [`Worker::seek`].
    streaming: Option<Arc<Growth>>,
    queue: Arc<[Track]>,
    /// Index into `queue` of the track that is loaded.
    index: usize,
    volume: u8,
    shuffle: bool,
    repeat: Repeat,
    /// The order the queue is played in: 0..len, or a shuffle of it.
    order: Vec<usize>,
    /// Tracks queued by hand that still wait right after the loaded one.
    queued: usize,
    /// Autoplay tracks that wait after those.
    auto: usize,
    rng: u64,
    /// Where the loaded track was last found in `order`. Checked before use, so any change
    /// of `order` only costs one scan.
    cursor: std::cell::Cell<usize>,
    /// The queue or the loaded track changed since the upcoming list was built.
    queue_dirty: bool,
}

/// xorshift64. Not for secrets; it only mixes a playlist.
fn next_rand(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

fn seed() -> u64 {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
    (t ^ 0x9E37_79B9_7F4A_7C15) | 1
}

/// Play order for a queue of `len` tracks. Shuffled orders start with `first`.
fn make_order(len: usize, first: Option<usize>, shuffle: bool, rng: &mut u64) -> Vec<usize> {
    let mut v: Vec<usize> = (0..len).collect();
    if !shuffle {
        return v;
    }
    for i in (1..len).rev() {
        let j = (next_rand(rng) % (i as u64 + 1)) as usize;
        v.swap(i, j);
    }
    if let Some(p) = first.and_then(|f| v.iter().position(|&x| x == f)) {
        v.swap(0, p);
    }
    v
}

impl Worker {
    fn new(snap: Arc<Mutex<Snapshot>>) -> Worker {
        let (sink, sink_error) = match DeviceSinkBuilder::open_default_sink() {
            Ok(mut s) => {
                s.log_on_drop(false);
                (Some(s), None)
            }
            Err(e) => (None, Some(format!("no audio output: {e}"))),
        };
        Worker {
            snap,
            sink,
            sink_error,
            player: None,
            streaming: None,
            queue: Arc::from(Vec::new()),
            index: 0,
            volume: 80,
            shuffle: false,
            repeat: Repeat::Off,
            order: Vec::new(),
            queued: 0,
            auto: 0,
            rng: seed(),
            cursor: std::cell::Cell::new(0),
            queue_dirty: true,
        }
    }

    fn run(mut self, rx: mpsc::Receiver<Request>) {
        loop {
            match rx.recv_timeout(TICK) {
                Ok(Request { msg: Msg::Shutdown, .. }) | Err(RecvTimeoutError::Disconnected) => return,
                Ok(Request { msg, deadline }) => {
                    // The caller stopped waiting: running the command now would change playback
                    // after the caller was told it failed.
                    if Instant::now() > deadline || msg.abandoned() {
                        msg.expire();
                    } else {
                        self.handle(msg);
                    }
                }
                Err(RecvTimeoutError::Timeout) => self.tick(),
            }
            self.publish();
        }
    }

    fn handle(&mut self, msg: Msg) {
        self.queue_dirty |= msg.changes_queue();
        match msg {
            Msg::PlayTrack { queue, index, reply } => {
                // The new queue replaces the old one only if its track loads. A bad file
                // leaves the old queue, order and sound as they were.
                let r = match self.prepare(&queue, index, None) {
                    Ok(p) => {
                        self.order = make_order(queue.len(), Some(index), self.shuffle, &mut self.rng);
                        self.queued = 0;
                        self.auto = 0;
                        self.queue = queue;
                        self.commit(p, index);
                        Ok(())
                    }
                    Err(e) => Err(e),
                };
                self.publish();
                let _ = reply.send(r);
            }
            Msg::Resume(reply) => {
                let r = match &self.player {
                    Some(p) => {
                        p.play();
                        Ok(())
                    }
                    // The queue ran out: Play starts the last song again.
                    None if self.queue.get(self.index).is_some() => self.load(self.index),
                    None => Err(Error::NotFound),
                };
                self.publish();
                let _ = reply.send(r);
            }
            Msg::Pause(reply) => {
                if let Some(p) = &self.player {
                    p.pause();
                }
                self.publish();
                let _ = reply.send(Ok(()));
            }
            Msg::Next(reply) => {
                let r = self.advance();
                self.publish();
                let _ = reply.send(r);
            }
            Msg::Enqueue { track, place, reply } => {
                let r = self.enqueue(track, place);
                self.publish();
                let _ = reply.send(r);
            }
            Msg::Jump { uri, reply } => {
                let r = self.jump(&uri);
                self.publish();
                let _ = reply.send(r);
            }
            Msg::Previous(reply) => {
                let restart = self.player.as_ref().is_some_and(|p| p.get_pos() > RESTART_AFTER);
                let pos = self.pos();
                let target = if restart || pos == 0 { self.index } else { self.order[pos - 1] };
                let queue = self.queue.clone();
                let r = self.prepare(&queue, target, None).map(|p| {
                    self.queued = 0;
                    self.auto = 0;
                    self.commit(p, target);
                });
                self.publish();
                let _ = reply.send(r);
            }
            Msg::Seek(ms, reply) => {
                let r = self.seek(ms);
                self.publish();
                let _ = reply.send(r);
            }
            Msg::Volume(v) => {
                self.volume = v;
                if let Some(p) = &self.player {
                    p.set_volume(gain(v));
                }
            }
            Msg::Shuffle(on, reply) => {
                self.shuffle = on;
                self.queued = 0;
                self.auto = 0;
                // The loaded track stays first, so turning it on never skips.
                self.order = make_order(self.queue.len(), Some(self.index), on, &mut self.rng);
                self.publish();
                let _ = reply.send(Ok(()));
            }
            Msg::Repeat(mode, reply) => {
                self.repeat = mode;
                self.publish();
                let _ = reply.send(Ok(()));
            }
            Msg::Shutdown => {}
        }
    }

    fn enqueue(&mut self, track: Track, place: Place) -> Result<()> {
        // Nothing loaded: the track simply starts.
        if self.player.is_none() && self.queue.is_empty() {
            let queue: Arc<[Track]> = Arc::from(vec![track]);
            let p = self.prepare(&queue, 0, None)?;
            self.queue = queue;
            self.order = vec![0];
            self.queued = 0;
            self.auto = 0;
            self.commit(p, 0);
            return Ok(());
        }
        let mut list: Vec<Track> = self.queue.to_vec();
        list.push(track);
        let id = list.len() - 1;
        self.queue = list.into();
        let pos = if self.order.is_empty() { 0 } else { self.pos() + 1 };
        let at = match place {
            Place::Next => pos,
            Place::Queue => pos + self.queued,
            Place::Auto => pos + self.queued + self.auto,
        }
        .min(self.order.len());
        self.order.insert(at, id);
        match place {
            Place::Next | Place::Queue => self.queued += 1,
            Place::Auto => self.auto += 1,
        }
        Ok(())
    }

    /// Plays the waiting song `uri` now. The songs between the loaded one and it count as
    /// passed over: they leave the queued and autoplay blocks, and stay behind for Previous.
    fn jump(&mut self, uri: &str) -> Result<()> {
        let pos = self.pos();
        let ahead = self.order.iter().skip(pos + 1).position(|&i| self.queue.get(i).is_some_and(|t| t.uri.as_ref() == uri));
        let Some(offset) = ahead else { return Err(Error::NotFound) };
        let target_pos = pos + 1 + offset;
        let target = self.order[target_pos];
        let queue = self.queue.clone();
        let p = self.prepare(&queue, target, None)?;
        let mut passed = offset + 1;
        let from_queued = passed.min(self.queued);
        self.queued -= from_queued;
        passed -= from_queued;
        self.auto = self.auto.saturating_sub(passed);
        self.commit(p, target);
        self.cursor.set(target_pos);
        self.compact();
        Ok(())
    }

    /// Forgets songs far behind the loaded one, so a long session does not keep every song it
    /// ever played (and copy the whole list on every enqueue).
    fn compact(&mut self) {
        if self.queue.len() <= MAX_QUEUE {
            return;
        }
        let keep_from = self.pos().saturating_sub(KEEP_BEHIND);
        let kept: Vec<usize> = self.order[keep_from..].to_vec();
        let mut remap: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
        let mut queue: Vec<Track> = Vec::with_capacity(kept.len());
        for &old in &kept {
            if let std::collections::hash_map::Entry::Vacant(e) = remap.entry(old) {
                e.insert(queue.len());
                queue.push(self.queue[old].clone());
            }
        }
        let Some(&index) = remap.get(&self.index) else { return };
        self.order = kept.iter().map(|o| remap[o]).collect();
        self.index = index;
        self.queue = queue.into();
    }

    /// Where the loaded track is in the play order.
    fn pos(&self) -> usize {
        let at = self.cursor.get();
        if self.order.get(at) == Some(&self.index) {
            return at;
        }
        let found = self.order.iter().position(|&i| i == self.index).unwrap_or(0);
        self.cursor.set(found);
        found
    }

    /// Moves to the next track. At the end of the queue it starts over when the
    /// whole queue repeats, and stops otherwise. A file that cannot be played is stepped
    /// over (a few in a row); the sound and the position change only when a track loads,
    /// so the loaded track and what is published always agree.
    fn advance(&mut self) -> Result<()> {
        let (mut queued, mut auto) = (self.queued, self.auto);
        let mut at = self.pos();
        let queue = self.queue.clone();
        let mut skipped = None;
        for _ in 0..MAX_SKIPS {
            let next = match self.order.get(at + 1) {
                Some(&i) => {
                    at += 1;
                    i
                }
                None if self.repeat == Repeat::Context && !self.order.is_empty() => {
                    if self.shuffle {
                        self.order = make_order(queue.len(), None, true, &mut self.rng);
                        // Do not play the same track twice in a row.
                        if self.order.len() > 1 && self.order[0] == self.index {
                            self.order.swap(0, 1);
                        }
                    }
                    queued = 0;
                    auto = 0;
                    at = 0;
                    self.order[0]
                }
                None if skipped.is_some() => break,
                None => {
                    self.player = None;
                    return Ok(());
                }
            };
            // The song that plays next is the first of the queued ones, then of the autoplay ones.
            if queued > 0 {
                queued -= 1;
            } else {
                auto = auto.saturating_sub(1);
            }
            match self.prepare(&queue, next, None) {
                Ok(p) => {
                    self.queued = queued;
                    self.auto = auto;
                    self.commit(p, next);
                    self.compact();
                    return Ok(());
                }
                Err(e) => skipped = Some(e),
            }
        }
        Err(skipped.unwrap_or(Error::NotFound))
    }

    fn tick(&mut self) {
        let finished = self.player.as_ref().is_some_and(|p| p.empty() && !p.is_paused());
        if finished {
            self.queue_dirty = true;
            if self.repeat == Repeat::Track && self.load(self.index).is_ok() {
                return;
            }
            // `advance` steps over bad files. When none of the next ones plays, the
            // queue has ended for now.
            if self.advance().is_err() {
                self.player = None;
            }
        }
    }

    /// Loads track `index` of the current queue and starts it.
    fn load(&mut self, index: usize) -> Result<()> {
        let queue = self.queue.clone();
        let p = self.prepare(&queue, index, None)?;
        self.commit(p, index);
        Ok(())
    }

    /// Opens track `index` of `queue` as a paused player, at `start` when given. Nothing of
    /// the worker changes: the caller commits the result, or drops it when it fails.
    fn prepare(&self, queue: &[Track], index: usize, start: Option<Duration>) -> Result<Prepared> {
        let sink = match (&self.sink, &self.sink_error) {
            (Some(s), _) => s,
            (None, e) => return Err(Error::Decode(e.clone().unwrap_or_else(|| "no audio output".into()).into())),
        };
        let track = queue.get(index).ok_or(Error::NotFound)?;
        let file = File::open(track.uri.as_ref()).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Error::NotFound,
            _ => Error::Decode(format!("open {}: {e}", track.uri).into()),
        })?;
        let player = Player::connect_new(sink.mixer());
        player.set_volume(gain(self.volume));
        player.pause();
        // A file that is still downloading is read as it grows. Symphonia needs an
        // unseekable source for that: a seekable one is scanned to its end first.
        let growing = growth::lookup(std::path::Path::new(track.uri.as_ref())).filter(|g| !g.is_done());
        let streaming = match growing {
            Some(g) => {
                let reader = GrowingReader::new(file, g.clone());
                // Building the decoder blocks this thread until the first bytes arrive. Stop
                // waiting before the caller's deadline, so a slow start fails the command
                // instead of freezing the worker and then changing playback after the fact.
                let give_up = reader.give_up();
                *give_up.lock().unwrap() = Some(Instant::now() + BUILD_WAIT);
                let built = Decoder::builder().with_data(reader).with_hint("m4a").with_seekable(false).build();
                *give_up.lock().unwrap() = None;
                let decoder = built.map_err(|e| Error::Decode(format!("decode {}: {e}", track.name).into()))?;
                player.append(decoder);
                Some(g)
            }
            None => {
                let decoder =
                    Decoder::try_from(file).map_err(|e| Error::Decode(format!("decode {}: {e}", track.name).into()))?;
                player.append(decoder);
                None
            }
        };
        if let Some(at) = start {
            player.try_seek(at).map_err(|e| Error::Decode(format!("seek failed: {e}").into()))?;
        }
        Ok(Prepared { player, streaming })
    }

    /// Makes a prepared player the loaded one and starts it. The old player stops when dropped.
    fn commit(&mut self, p: Prepared, index: usize) {
        p.player.play();
        self.player = Some(p.player);
        self.streaming = p.streaming;
        self.index = index;
    }

    /// Moves to `ms` in the loaded track. A track that still plays from a growing
    /// file cannot seek; once the download is done it is opened again as an
    /// ordinary file, and the seek is made there.
    fn seek(&mut self, ms: u32) -> Result<()> {
        let at = Duration::from_millis(ms as u64);
        if let Some(g) = self.streaming.clone() {
            if !g.is_done() {
                return Err(Error::Decode("still downloading; seek again in a moment".into()));
            }
            let was_paused = self.player.as_ref().is_some_and(|p| p.is_paused());
            // The file is complete now, so this builds the seekable decoder.
            let queue = self.queue.clone();
            let p = self.prepare(&queue, self.index, Some(at))?;
            self.commit(p, self.index);
            if was_paused {
                if let Some(p) = &self.player {
                    p.pause();
                }
            }
            return Ok(());
        }
        match &self.player {
            Some(p) => p.try_seek(at).map_err(|e| Error::Decode(format!("seek failed: {e}").into())),
            None => Err(Error::NotFound),
        }
    }

    fn publish(&mut self) {
        let rebuild = std::mem::take(&mut self.queue_dirty);
        let mut s = self.snap.lock().unwrap();
        s.volume = self.volume;
        s.shuffle = self.shuffle;
        s.repeat = self.repeat;
        match &self.player {
            Some(p) => {
                s.ended = false;
                s.current = self.queue.get(self.index).cloned();
                s.playing = !p.is_paused() && !p.empty();
                s.progress_ms = p.get_pos().as_millis().min(u32::MAX as u128) as u32;
                // The list of upcoming songs only changes with the queue, not with the clock.
                if rebuild {
                    let upcoming: Arc<[Track]> =
                        self.order.iter().skip(self.pos() + 1).take(UPCOMING).filter_map(|&i| self.queue.get(i).cloned()).collect();
                    s.manual = self.queued.min(upcoming.len());
                    s.auto = self.auto.min(upcoming.len() - s.manual);
                    s.upcoming = upcoming;
                    s.queue_rev += 1;
                }
            }
            None => {
                s.ended = !self.queue.is_empty();
                s.playing = false;
                s.progress_ms = 0;
                if !s.upcoming.is_empty() || rebuild {
                    s.upcoming = Arc::from(Vec::new());
                    s.manual = 0;
                    s.auto = 0;
                    s.queue_rev += 1;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(n: &str) -> Track {
        Track {
            id: n.into(),
            uri: n.into(),
            name: n.into(),
            artists: Arc::from(Vec::new()),
            album: None,
            duration_ms: 0,
        }
    }

    #[test]
    fn queued_tracks_go_after_the_loaded_one_in_the_order_added() {
        let mut w = Worker::new(Arc::new(Mutex::new(Snapshot::default())));
        w.queue = Arc::from(vec![track("a"), track("b"), track("c")]);
        w.order = vec![0, 1, 2];
        w.index = 0;
        // The queue is not empty, so inserting needs no audio device.
        w.enqueue(track("x"), Place::Queue).unwrap();
        w.enqueue(track("y"), Place::Queue).unwrap();
        w.enqueue(track("z"), Place::Next).unwrap();
        let names: Vec<String> = w.order.iter().map(|&i| w.queue[i].name.to_string()).collect();
        assert_eq!(names, ["a", "z", "x", "y", "b", "c"]);
        assert_eq!(w.queued, 3);
    }

    #[test]
    fn songs_queued_by_hand_come_before_autoplay_songs() {
        let mut w = Worker::new(Arc::new(Mutex::new(Snapshot::default())));
        w.queue = Arc::from(vec![track("a")]);
        w.order = vec![0];
        w.index = 0;
        w.enqueue(track("auto1"), Place::Auto).unwrap();
        w.enqueue(track("auto2"), Place::Auto).unwrap();
        // The user queues a song after autoplay did: it still plays first.
        w.enqueue(track("mine"), Place::Queue).unwrap();
        w.enqueue(track("auto3"), Place::Auto).unwrap();
        let names: Vec<String> = w.order.iter().map(|&i| w.queue[i].name.to_string()).collect();
        assert_eq!(names, ["a", "mine", "auto1", "auto2", "auto3"]);
        assert_eq!((w.queued, w.auto), (1, 3));
    }

    #[test]
    fn jumping_passes_over_the_songs_before_the_target() {
        let mut w = Worker::new(Arc::new(Mutex::new(Snapshot::default())));
        w.queue = Arc::from(vec![track("a"), track("b"), track("c"), track("d")]);
        w.order = vec![0, 1, 2, 3];
        w.index = 0;
        w.enqueue(track("m1"), Place::Queue).unwrap();
        w.enqueue(track("x1"), Place::Auto).unwrap();
        w.enqueue(track("x2"), Place::Auto).unwrap();
        // order: a m1 x1 x2 b c d. Jump to x2 would load a file: only the counting is checked.
        let before = (w.queued, w.auto);
        assert_eq!(before, (1, 2));
        assert!(matches!(w.jump("nothing"), Err(Error::NotFound)), "unknown songs are refused");
        assert_eq!((w.queued, w.auto), before, "a refused jump changes nothing");
    }

    #[test]
    fn a_long_queue_is_trimmed_behind_the_playing_song() {
        let mut w = Worker::new(Arc::new(Mutex::new(Snapshot::default())));
        let tracks: Vec<Track> = (0..1500).map(|n| track(&n.to_string())).collect();
        w.queue = tracks.into();
        w.order = (0..1500).collect();
        w.index = 1200;
        w.compact();
        assert_eq!(w.queue.len(), 1500 - (1200 - KEEP_BEHIND));
        assert_eq!(w.queue[w.index].name.as_ref(), "1200");
        assert_eq!(w.order[w.pos() - 1], w.index - 1, "Previous still works");
        let next = w.order[w.pos() + 1];
        assert_eq!(w.queue[next].name.as_ref(), "1201");
    }

    #[test]
    fn only_commands_that_change_the_queue_mark_it_changed() {
        let reply = || oneshot::channel().0;
        assert!(!Msg::Pause(reply()).changes_queue());
        assert!(!Msg::Seek(5, reply()).changes_queue());
        assert!(!Msg::Repeat(Repeat::Off, reply()).changes_queue());
        assert!(!Msg::Volume(10).changes_queue());
        assert!(Msg::Next(reply()).changes_queue());
        assert!(Msg::Shuffle(true, reply()).changes_queue());
        assert!(Msg::Jump { uri: "x".into(), reply: reply() }.changes_queue());
    }

    #[test]
    fn volume_follows_a_curve_that_is_quiet_at_the_bottom() {
        assert_eq!(gain(0), 0.0);
        assert_eq!(gain(100), 1.0);
        assert_eq!(gain(200), 1.0, "limited to 100");
        assert!(gain(9) < 0.001, "9% is nearly silent: {}", gain(9));
        assert!(gain(50) > 0.1 && gain(50) < 0.15);
        assert!((1..=100u8).all(|v| gain(v) > gain(v - 1)), "louder at every step");
    }

    #[test]
    fn plain_order_is_the_queue_order() {
        let mut rng = 7;
        assert_eq!(make_order(4, Some(2), false, &mut rng), vec![0, 1, 2, 3]);
        assert!(make_order(0, None, true, &mut rng).is_empty());
    }

    #[test]
    fn shuffle_keeps_every_track_once_and_the_loaded_one_first() {
        for start in [0usize, 3, 9] {
            let mut rng = seed();
            let order = make_order(10, Some(start), true, &mut rng);
            assert_eq!(order[0], start);
            let mut sorted = order.clone();
            sorted.sort_unstable();
            assert_eq!(sorted, (0..10).collect::<Vec<_>>());
        }
    }

    #[test]
    fn shuffle_actually_mixes() {
        let mut rng = 12345;
        let order = make_order(50, None, true, &mut rng);
        assert_ne!(order, (0..50).collect::<Vec<_>>());
    }
}
