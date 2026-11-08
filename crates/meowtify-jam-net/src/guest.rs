//! A guest of a jam: joins the room on the relay, builds the direct link to the host, receives
//! the audio into a cache folder, and tells the app what to do to stay in step.
//!
//! The app reports what its player is doing ([`Guest::report`]) and carries out the
//! [`GuestEvent::Do`] steps. All the deciding is in [`meowtify_jam::follower`].

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use meowtify_jam::follower::{Follower, Local, Step};
use meowtify_jam::proto::{parse_audio_frame, Action, HostState, Perms, Signal, SongId, SongRef, ToGuest, ToHost};
use meowtify_jam::{ClientMsg, Member, ServerMsg, Target};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use crate::link::{Event as LinkEvent, Link, Options};
use crate::relay::{Relay, RelayConfig, RelayEvent};

/// Complete songs kept in the cache folder. Older ones are deleted.
const KEEP_FILES: usize = 8;
/// The biggest song a guest accepts. A bigger one is refused, not stored.
const MAX_SONG_BYTES: u64 = 200 * 1024 * 1024;
const TICK: Duration = Duration::from_secs(1);
/// File name prefix in the cache folder, so leftovers can be found and removed.
pub const CACHE_PREFIX: &str = "jam-";

/// Finds the guest's own copy of a song the host plays, so it is not sent again. It gets what
/// the host says about the song and answers with a file that exists, or `None`. It runs on the
/// guest's task, so it must be quick. The file is only played, never changed or deleted.
#[derive(Clone)]
pub struct LocalCopy(pub Arc<dyn Fn(&SongRef) -> Option<PathBuf> + Send + Sync>);

impl std::fmt::Debug for LocalCopy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LocalCopy")
    }
}

#[derive(Debug, Clone)]
pub struct GuestConfig {
    pub relay: RelayConfig,
    pub link: Options,
    /// Where songs are stored while the jam lasts.
    pub cache_dir: PathBuf,
    /// Looks for songs the guest already has. `None` asks the host for every song.
    pub local_copy: Option<LocalCopy>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GuestEvent {
    /// Who is in the jam.
    Members(Vec<Member>),
    /// The direct connection to the host opened or closed.
    Linked(bool),
    /// The host's state, for showing what plays and what is next.
    State(HostState),
    Perms(Perms),
    /// Do this to the player to stay with the host.
    Do(Step),
    /// The host refused a request.
    Denied(String),
    /// The relay connection dropped, with the reason. It is trying again.
    Reconnecting(String),
    /// The jam is over for the guest.
    Down(String),
}

enum Cmd {
    Report(Local),
    Request(Action),
    Leave,
}

pub struct Guest {
    cmds: mpsc::UnboundedSender<Cmd>,
    task: AbortHandle,
    cache_dir: PathBuf,
}

impl Drop for Guest {
    fn drop(&mut self) {
        self.task.abort();
        remove_cache(&self.cache_dir);
    }
}

impl Guest {
    pub fn join(cfg: GuestConfig) -> (Guest, mpsc::UnboundedReceiver<GuestEvent>) {
        let (cmds_tx, cmds) = mpsc::unbounded_channel();
        let (events_tx, events) = mpsc::unbounded_channel();
        let cache_dir = cfg.cache_dir.clone();
        let task = tokio::spawn(Actor::new(cfg, events_tx).run(cmds)).abort_handle();
        (Guest { cmds: cmds_tx, task, cache_dir }, events)
    }

    /// What the guest's player does now. Call it when it changes and about once a second.
    pub fn report(&self, local: Local) {
        let _ = self.cmds.send(Cmd::Report(local));
    }

    /// Asks the host for something. The host decides; a refusal comes back as `Denied`.
    pub fn request(&self, action: Action) {
        let _ = self.cmds.send(Cmd::Request(action));
    }

    pub fn leave(&self) {
        let _ = self.cmds.send(Cmd::Leave);
    }
}

/// Deletes the songs of past jams from `dir`.
pub fn remove_cache(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        if e.file_name().to_string_lossy().starts_with(CACHE_PREFIX) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// A song being received.
struct Incoming {
    file: tokio::fs::File,
    path: PathBuf,
    written: u64,
    /// Known once the host says the song is complete.
    size: Option<u64>,
}

struct LinkMsg {
    generation: u64,
    event: LinkEvent,
}

struct Actor {
    cfg: GuestConfig,
    events: mpsc::UnboundedSender<GuestEvent>,
    start: Instant,
    follower: Follower,
    local: Local,
    link: Option<Arc<Link>>,
    /// The direct connection is open.
    open: bool,
    generation: u64,
    incoming: HashMap<SongId, Incoming>,
    /// Complete files, oldest first.
    done: VecDeque<(SongId, PathBuf)>,
    /// The one song the host is sending now. Sending one at a time, the playing song first,
    /// gets that song here soonest: songs that share the channel each get a share of it.
    active: Option<SongId>,
    /// Songs in `incoming` that wait for their turn, in the order they were wanted. A song that
    /// was paused for a more urgent one keeps what it has and goes on from there.
    waiting: VecDeque<SongId>,
    pings_sent: u32,
}

impl Actor {
    fn new(cfg: GuestConfig, events: mpsc::UnboundedSender<GuestEvent>) -> Actor {
        Actor {
            cfg,
            events,
            start: Instant::now(),
            follower: Follower::new(),
            local: Local::default(),
            link: None,
            open: false,
            generation: 0,
            incoming: HashMap::new(),
            done: VecDeque::new(),
            active: None,
            waiting: VecDeque::new(),
            pings_sent: 0,
        }
    }

    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    fn emit(&self, e: GuestEvent) {
        let _ = self.events.send(e);
    }

    async fn run(mut self, mut cmds: mpsc::UnboundedReceiver<Cmd>) {
        let _ = tokio::fs::create_dir_all(&self.cfg.cache_dir).await;
        remove_cache(&self.cfg.cache_dir);
        let (relay, mut relay_events) = Relay::connect(self.cfg.relay.clone());
        let (link_tx, mut link_events) = mpsc::unbounded_channel::<LinkMsg>();
        let mut tick = tokio::time::interval(TICK);
        loop {
            tokio::select! {
                ev = relay_events.recv() => {
                    let Some(ev) = ev else { return };
                    match ev {
                        RelayEvent::Msg(msg) => self.on_relay(&relay, &link_tx, msg).await,
                        RelayEvent::Reconnecting(why) => self.emit(GuestEvent::Reconnecting(why)),
                        RelayEvent::Down(why) => {
                            self.emit(GuestEvent::Down(why));
                            return;
                        }
                    }
                }
                cmd = cmds.recv() => {
                    let Some(cmd) = cmd else { return };
                    match cmd {
                        Cmd::Report(local) => self.local = local,
                        Cmd::Request(action) => {
                            if let Some(link) = self.link.as_ref().filter(|_| self.open) {
                                send_control(link, &ToHost::Request(action)).await;
                            }
                        }
                        Cmd::Leave => {
                            self.emit(GuestEvent::Down("you left the jam".into()));
                            return;
                        }
                    }
                }
                msg = link_events.recv() => {
                    if let Some(msg) = msg { self.on_link(&relay, msg).await; }
                }
                _ = tick.tick() => {
                    let steps = self.follower.on_tick(self.now_ms(), &self.local);
                    self.apply(steps).await;
                    self.ping().await;
                }
            }
        }
    }

    async fn on_relay(&mut self, relay: &Relay, link_tx: &mpsc::UnboundedSender<LinkMsg>, msg: ServerMsg) {
        match msg {
            ServerMsg::Welcome { members, .. } => self.emit(GuestEvent::Members(members)),
            ServerMsg::Msg { body, .. } => {
                let Ok(signal) = serde_json::from_value::<Signal>(body) else { return };
                match signal {
                    Signal::Offer { sdp } => self.on_offer(relay, link_tx, &sdp).await,
                    Signal::Candidate { json } => {
                        if let Some(link) = &self.link {
                            let _ = link.add_candidate(&json).await;
                        }
                    }
                    Signal::Answer { .. } => {}
                }
            }
            _ => {}
        }
    }

    async fn on_offer(&mut self, relay: &Relay, link_tx: &mpsc::UnboundedSender<LinkMsg>, sdp: &str) {
        // A new offer replaces any earlier link: the host starts over when its side broke.
        if let Some(old) = self.link.take() {
            old.close().await;
        }
        if self.open {
            self.open = false;
            self.emit(GuestEvent::Linked(false));
        }
        self.generation += 1;
        let generation = self.generation;
        let Ok((mut link, answer)) = Link::answer(self.cfg.link.clone(), sdp).await else { return };
        let Some(mut events) = link.take_events() else { return };
        self.link = Some(Arc::new(link));
        self.pings_sent = 0;
        let tx = link_tx.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if tx.send(LinkMsg { generation, event }).is_err() {
                    break;
                }
            }
        });
        signal(relay, &Signal::Answer { sdp: answer });
    }

    async fn on_link(&mut self, relay: &Relay, msg: LinkMsg) {
        if msg.generation != self.generation {
            return; // a link that was replaced
        }
        match msg.event {
            LinkEvent::Candidate(json) => signal(relay, &Signal::Candidate { json }),
            LinkEvent::Open => {
                self.open = true;
                self.emit(GuestEvent::Linked(true));
                // Songs that were wanted before the link was up, or when it dropped: the host
                // knows nothing of them, so the first one is asked for again.
                self.requeue_active();
                self.pump().await;
                self.ping().await;
            }
            LinkEvent::Closed => {
                self.link = None;
                self.requeue_active();
                if self.open {
                    self.open = false;
                    self.emit(GuestEvent::Linked(false));
                }
            }
            LinkEvent::Control(bytes) => {
                if let Ok(msg) = serde_json::from_slice::<ToGuest>(&bytes) {
                    self.on_host(msg).await;
                }
            }
            LinkEvent::Audio(bytes) => {
                if let Some((song, offset, data)) = parse_audio_frame(&bytes) {
                    self.on_audio(song, offset, data).await;
                }
            }
        }
    }

    async fn on_host(&mut self, msg: ToGuest) {
        match msg {
            ToGuest::State(state) => {
                self.emit(GuestEvent::State(state.clone()));
                let steps = self.follower.on_state(state, self.now_ms(), &self.local);
                self.apply(steps).await;
            }
            ToGuest::Pong { sent_ms, host_ms } => {
                let now = self.now_ms();
                self.follower.clock_mut().sample(sent_ms, now, host_ms);
            }
            ToGuest::Perms(p) => self.emit(GuestEvent::Perms(p)),
            ToGuest::AudioEnd { song, size } => {
                if size > MAX_SONG_BYTES {
                    self.discard(song).await;
                    return;
                }
                if let Some(i) = self.incoming.get_mut(&song) {
                    i.size = Some(size);
                }
                self.finish_if_complete(song).await;
            }
            ToGuest::NoSong { song } => self.discard(song).await,
            ToGuest::Denied { what } => self.emit(GuestEvent::Denied(what)),
        }
    }

    async fn on_audio(&mut self, song: SongId, offset: u64, data: &[u8]) {
        let Some(i) = self.incoming.get_mut(&song) else { return };
        // The channel is ordered. A frame that starts before what is written (a transfer that
        // was paused and resumed overlaps its last frames) is used from where it is new; a frame
        // that starts after it leaves a gap, so it is a stale transfer and is dropped.
        let Some(skip) = overlap_skip(offset, i.written, data.len()) else { return };
        let data = &data[skip..];
        if i.written + data.len() as u64 > MAX_SONG_BYTES || i.file.write_all(data).await.is_err() {
            self.discard(song).await;
            return;
        }
        i.written += data.len() as u64;
        self.finish_if_complete(song).await;
    }

    /// Drops a song that cannot be had, and lets the follower ask again later.
    async fn discard(&mut self, song: SongId) {
        if let Some(i) = self.incoming.remove(&song) {
            drop(i.file);
            let _ = tokio::fs::remove_file(&i.path).await;
        }
        self.forget_transfer(song);
        self.follower.on_missing(song);
        self.pump().await;
    }

    /// The song is no longer being received: it is done or gone.
    fn forget_transfer(&mut self, song: SongId) {
        if self.active == Some(song) {
            self.active = None;
        }
        self.waiting.retain(|s| *s != song);
    }

    /// The host is not sending the active song any more (a new link, or none): it waits first.
    fn requeue_active(&mut self) {
        if let Some(song) = self.active.take() {
            if self.incoming.contains_key(&song) {
                self.waiting.push_front(song);
            }
        }
    }

    /// Makes the host send the song that should come next, and only that one. The song that
    /// plays is first; otherwise the one in progress goes on, then the others in the order
    /// they were wanted. A song that gives way is told to stop, and resumes where it was.
    async fn pump(&mut self) {
        let Some(link) = self.link.clone().filter(|_| self.open) else { return };
        let current = self.follower.state().and_then(|s| s.song.as_ref()).map(|s| s.id);
        let Some(next) = next_transfer(current, self.active, &self.waiting, |s| self.incoming.contains_key(&s)) else { return };
        if self.active == Some(next) {
            return;
        }
        if let Some(old) = self.active.take() {
            send_control(&link, &ToHost::Cancel { song: old }).await;
            if self.incoming.contains_key(&old) {
                self.waiting.push_front(old);
            }
        }
        self.waiting.retain(|s| *s != next);
        let Some(from) = self.incoming.get(&next).map(|i| i.written) else { return };
        self.active = Some(next);
        send_control(&link, &ToHost::Want { song: next, from }).await;
    }

    async fn finish_if_complete(&mut self, song: SongId) {
        let ready = self.incoming.get(&song).is_some_and(|i| i.size == Some(i.written));
        if !ready {
            return;
        }
        let Some(mut i) = self.incoming.remove(&song) else { return };
        let _ = i.file.flush().await;
        drop(i.file);
        let path = self.cfg.cache_dir.join(format!("{CACHE_PREFIX}{song}.audio"));
        if tokio::fs::rename(&i.path, &path).await.is_err() {
            let _ = tokio::fs::remove_file(&i.path).await;
            self.follower.on_missing(song);
            return;
        }
        self.forget_transfer(song);
        self.done.push_back((song, path.clone()));
        let steps = self.follower.on_file(song, path.to_string_lossy().into_owned(), self.now_ms(), &self.local);
        self.apply(steps).await;
        self.evict().await;
    }

    /// Deletes the oldest complete songs past [`KEEP_FILES`], never the one that plays.
    async fn evict(&mut self) {
        let playing = self.follower.state().and_then(|s| s.song.as_ref()).map(|s| s.id);
        while self.done.len() > KEEP_FILES {
            let Some(at) = self.done.iter().position(|(s, _)| Some(*s) != playing) else { break };
            if let Some((song, path)) = self.done.remove(at) {
                let _ = tokio::fs::remove_file(&path).await;
                self.follower.forget(song);
            }
        }
    }

    async fn apply(&mut self, steps: Vec<Step>) {
        let mut todo: VecDeque<Step> = steps.into();
        while let Some(step) = todo.pop_front() {
            match step {
                Step::Want(song) => match self.own_copy(song) {
                    // The guest has this song: play that file. What the follower plans next
                    // (more songs to want) is handled in the same loop.
                    Some(path) => {
                        let path = path.to_string_lossy().into_owned();
                        todo.extend(self.follower.on_file(song, path, self.now_ms(), &self.local));
                    }
                    None => self.want(song).await,
                },
                other => self.emit(GuestEvent::Do(other)),
            }
        }
        self.pump().await;
    }

    /// The guest's own file for a song the host wants it to have, if there is one.
    fn own_copy(&self, song: SongId) -> Option<PathBuf> {
        let find = &self.cfg.local_copy.as_ref()?.0;
        let state = self.follower.state()?;
        let wanted = state.song.iter().chain(state.queue.iter()).find(|s| s.id == song)?;
        find(wanted).filter(|p| p.is_file())
    }

    /// Opens a file for a song and puts it in line. The host is asked when its turn comes.
    async fn want(&mut self, song: SongId) {
        if self.incoming.contains_key(&song) {
            return;
        }
        let path = self.cfg.cache_dir.join(format!("{CACHE_PREFIX}{song}.part"));
        let Ok(file) = tokio::fs::File::create(&path).await else {
            self.follower.on_missing(song);
            return;
        };
        self.incoming.insert(song, Incoming { file, path, written: 0, size: None });
        self.waiting.push_back(song);
    }

    /// Measures the host's clock: five quick pings after the link opens, then one every ten seconds.
    async fn ping(&mut self) {
        let Some(link) = self.link.clone().filter(|_| self.open) else { return };
        let due = self.pings_sent < 5 || self.pings_sent % 10 == 0;
        self.pings_sent += 1;
        if due {
            send_control(&link, &ToHost::Ping { sent_ms: self.now_ms() }).await;
        }
    }
}

/// How many bytes to skip at the start of an audio frame that begins at `offset`, when `written`
/// bytes of the song are saved. `None` when the frame leaves a gap or has nothing new.
fn overlap_skip(offset: u64, written: u64, len: usize) -> Option<usize> {
    if offset == written {
        Some(0)
    } else if offset < written && offset + len as u64 > written {
        Some((written - offset) as usize)
    } else {
        None
    }
}

/// The song the host should be sending: the playing one when it is still being received, else
/// the one in progress, else the first that waits.
fn next_transfer(current: Option<SongId>, active: Option<SongId>, waiting: &VecDeque<SongId>, wanted: impl Fn(SongId) -> bool) -> Option<SongId> {
    current
        .filter(|s| wanted(*s))
        .or(active.filter(|s| wanted(*s)))
        .or_else(|| waiting.iter().copied().find(|s| wanted(*s)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_that_overlaps_what_is_saved_is_used_from_where_it_is_new() {
        assert_eq!(overlap_skip(100, 100, 50), Some(0), "the next frame");
        assert_eq!(overlap_skip(80, 100, 50), Some(20), "a resumed transfer starts a little back");
        assert_eq!(overlap_skip(50, 100, 50), None, "all of it is saved already");
        assert_eq!(overlap_skip(150, 100, 50), None, "a gap: stale");
    }

    #[test]
    fn the_playing_song_goes_first_then_the_one_in_progress_then_the_queue_in_order() {
        let waiting: VecDeque<SongId> = [3, 4].into();
        let all = |_: SongId| true;
        assert_eq!(next_transfer(Some(9), Some(3), &waiting, |s| s == 9 || s == 3 || s == 4), Some(9), "the playing song pushes ahead");
        assert_eq!(next_transfer(Some(9), Some(3), &waiting, |s| s != 9), Some(3), "the playing song is here already: go on");
        assert_eq!(next_transfer(None, Some(4), &waiting, all), Some(4), "no change in the middle of a song");
        assert_eq!(next_transfer(None, None, &waiting, all), Some(3));
        assert_eq!(next_transfer(None, None, &VecDeque::new(), all), None);
        assert_eq!(next_transfer(Some(9), None, &VecDeque::new(), |_| false), None);
    }
}

fn signal(relay: &Relay, signal: &Signal) {
    if let Ok(body) = serde_json::to_value(signal) {
        relay.send(ClientMsg::Send { to: Target::Host, body });
    }
}

async fn send_control(link: &Link, msg: &ToHost) {
    if let Ok(bytes) = serde_json::to_vec(msg) {
        let _ = link.send_control(&bytes).await;
    }
}
