//! The host of a jam: keeps the room open on the relay, builds a direct link to each guest,
//! tells them what plays, and serves them the audio.
//!
//! The host is the authority. A guest's request is carried out only when the host's rules for
//! that guest allow it, and guests learn song numbers, never file paths.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use meowtify_core::growth;
use meowtify_jam::proto::{audio_frame, Action, HostState, Perms, SharedLyrics, Signal, SongId, SongRef, ToGuest, ToHost, AUDIO_HEADER, MAX_QUEUE};
use meowtify_jam::{ClientMsg, Member, MemberId, ServerMsg, Target};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use crate::link::{Event as LinkEvent, Link, Options, CHUNK};
use crate::relay::{Relay, RelayConfig, RelayEvent};

/// The state is sent again this often, so a guest that missed one catches up, and drift stays small.
const HEARTBEAT: Duration = Duration::from_secs(3);
/// How many songs the host remembers numbers for. A guest may still ask for one that just left the queue.
const KEEP_SONGS: usize = 64;
/// Link set-ups tried for one guest in a row before the host leaves it to the guest to rejoin.
const MAX_RETRIES: u32 = 3;
/// A guest may send this many requests per second on the control channel.
const REQUESTS_PER_SEC: u32 = 20;

/// A song the host can play, as the app describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SongInfo {
    pub path: PathBuf,
    pub title: String,
    pub artist: String,
    pub duration_ms: u32,
    /// The id in the file's name, if it has one. Sent to guests; see `SongRef::key`.
    pub key: Option<String>,
    /// Who queued the song; see `SongRef::by`.
    pub by: Option<u32>,
}

/// What the host's player is doing, whenever it changes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Snapshot {
    pub current: Option<SongInfo>,
    pub playing: bool,
    pub pos_ms: u32,
    pub queue: Vec<SongInfo>,
}

#[derive(Debug, Clone)]
pub struct HostConfig {
    pub relay: RelayConfig,
    pub link: Options,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberView {
    pub id: MemberId,
    pub name: String,
    /// The avatar colour the guest chose, in degrees.
    pub hue: Option<u16>,
    /// Connected to the relay right now.
    pub online: bool,
    /// The direct connection is open: the guest hears the jam.
    pub linked: bool,
    pub perms: Perms,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostEvent {
    /// The guests, whenever one joins, leaves, connects or gets other rights.
    Members(Vec<MemberView>),
    /// A guest with the right to control playback asked for this. The app carries it out.
    Do { from: String, action: Action },
    /// A guest suggested a song.
    Suggest { from: String, id: MemberId, text: String },
    /// The relay connection dropped, with the reason. It is trying again.
    Reconnecting(String),
    /// The jam is over for the host.
    Down(String),
}

enum Cmd {
    Publish(Snapshot),
    Lyrics(PathBuf, SharedLyrics),
    SetPerms(MemberId, Perms),
    Kick(MemberId),
    End,
}

pub struct Host {
    cmds: mpsc::UnboundedSender<Cmd>,
    task: AbortHandle,
}

impl Drop for Host {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Host {
    /// Opens the room and starts serving. Events come out of the receiver.
    pub fn start(cfg: HostConfig) -> (Host, mpsc::UnboundedReceiver<HostEvent>) {
        let (cmds_tx, cmds) = mpsc::unbounded_channel();
        let (events_tx, events) = mpsc::unbounded_channel();
        let task = tokio::spawn(Actor::new(cfg, events_tx).run(cmds)).abort_handle();
        (Host { cmds: cmds_tx, task }, events)
    }

    /// The player changed: tell the guests.
    pub fn publish(&self, snapshot: Snapshot) {
        let _ = self.cmds.send(Cmd::Publish(snapshot));
    }

    /// The words of the song at `path`, for the guests. Sent again to a guest that joins later.
    pub fn share_lyrics(&self, path: PathBuf, lyrics: SharedLyrics) {
        let _ = self.cmds.send(Cmd::Lyrics(path, lyrics.capped()));
    }

    pub fn set_perms(&self, id: MemberId, perms: Perms) {
        let _ = self.cmds.send(Cmd::SetPerms(id, perms));
    }

    pub fn kick(&self, id: MemberId) {
        let _ = self.cmds.send(Cmd::Kick(id));
    }

    /// Ends the jam for everyone.
    pub fn end(&self) {
        let _ = self.cmds.send(Cmd::End);
    }
}

/// Song numbers. The oldest are forgotten past [`KEEP_SONGS`].
#[derive(Default)]
struct Songs {
    next: SongId,
    by_path: HashMap<PathBuf, SongId>,
    by_id: HashMap<SongId, PathBuf>,
    order: VecDeque<SongId>,
}

impl Songs {
    fn id_of(&mut self, path: &PathBuf) -> SongId {
        if let Some(&id) = self.by_path.get(path) {
            return id;
        }
        let id = self.next;
        self.next += 1;
        self.by_path.insert(path.clone(), id);
        self.by_id.insert(id, path.clone());
        self.order.push_back(id);
        while self.order.len() > KEEP_SONGS {
            if let Some(old) = self.order.pop_front() {
                if let Some(p) = self.by_id.remove(&old) {
                    self.by_path.remove(&p);
                }
            }
        }
        id
    }

    fn reference(&mut self, s: &SongInfo) -> SongRef {
        SongRef { id: self.id_of(&s.path), title: s.title.clone(), artist: s.artist.clone(), duration_ms: s.duration_ms, key: s.key.clone(), by: s.by }
    }
}

struct Guest {
    name: String,
    hue: Option<u16>,
    online: bool,
    perms: Perms,
    link: Option<Arc<Link>>,
    /// Which link the events belong to; a newer link makes older events stale.
    generation: u64,
    open: bool,
    retries: u32,
    serving: HashMap<SongId, AbortHandle>,
    /// Requests seen in the current second.
    window: (u64, u32),
}

struct LinkMsg {
    guest: MemberId,
    generation: u64,
    event: LinkEvent,
}

struct Actor {
    cfg: HostConfig,
    events: mpsc::UnboundedSender<HostEvent>,
    start: Instant,
    guests: HashMap<MemberId, Guest>,
    songs: Songs,
    state: HostState,
    snapshot: Snapshot,
    snapshot_at: u64,
    next_generation: u64,
    /// The words of the song last shared, with its number.
    lyrics: Option<(SongId, SharedLyrics)>,
}

impl Actor {
    fn new(cfg: HostConfig, events: mpsc::UnboundedSender<HostEvent>) -> Actor {
        Actor {
            cfg,
            events,
            start: Instant::now(),
            guests: HashMap::new(),
            songs: Songs::default(),
            state: HostState::default(),
            snapshot: Snapshot::default(),
            snapshot_at: 0,
            next_generation: 1,
            lyrics: None,
        }
    }

    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    async fn run(mut self, mut cmds: mpsc::UnboundedReceiver<Cmd>) {
        let (relay, mut relay_events) = Relay::connect(self.cfg.relay.clone());
        let (link_tx, mut link_events) = mpsc::unbounded_channel::<LinkMsg>();
        let mut beat = tokio::time::interval(HEARTBEAT);
        loop {
            tokio::select! {
                ev = relay_events.recv() => {
                    let Some(ev) = ev else { return };
                    match ev {
                        RelayEvent::Msg(msg) => self.on_relay(&relay, &link_tx, msg).await,
                        RelayEvent::Reconnecting(why) => { let _ = self.events.send(HostEvent::Reconnecting(why)); }
                        RelayEvent::Down(why) => {
                            let _ = self.events.send(HostEvent::Down(why));
                            return;
                        }
                    }
                }
                cmd = cmds.recv() => {
                    let Some(cmd) = cmd else { return };
                    match cmd {
                        Cmd::Publish(s) => self.publish(s).await,
                        Cmd::Lyrics(path, lyrics) => {
                            let song = self.songs.id_of(&path);
                            self.lyrics = Some((song, lyrics.clone()));
                            let msg = ToGuest::Lyrics { song, lyrics };
                            let links: Vec<Arc<Link>> = self.guests.values().filter(|g| g.open).filter_map(|g| g.link.clone()).collect();
                            for link in links {
                                send_control(&link, &msg).await;
                            }
                        }
                        Cmd::SetPerms(id, perms) => {
                            if let Some(g) = self.guests.get_mut(&id) {
                                g.perms = perms;
                                if let Some(link) = g.link.clone().filter(|_| g.open) {
                                    send_control(&link, &ToGuest::Perms(perms)).await;
                                }
                                self.members_changed();
                            }
                        }
                        Cmd::Kick(id) => relay.send(ClientMsg::Kick { id }),
                        Cmd::End => {
                            relay.send(ClientMsg::End);
                            let _ = self.events.send(HostEvent::Down("you ended the jam".into()));
                            // Give the relay a moment to carry the message before the socket goes.
                            tokio::time::sleep(Duration::from_millis(200)).await;
                            return;
                        }
                    }
                }
                msg = link_events.recv() => {
                    if let Some(msg) = msg { self.on_link(&relay, &link_tx, msg).await; }
                }
                _ = beat.tick() => self.broadcast_state(false).await,
            }
        }
    }

    // ---- relay ----

    async fn on_relay(&mut self, relay: &Relay, link_tx: &mpsc::UnboundedSender<LinkMsg>, msg: ServerMsg) {
        match msg {
            ServerMsg::Welcome { members, .. } => {
                // First welcome, or the host is back after a drop: line the guests up again.
                for m in members.into_iter().filter(|m| !m.host) {
                    self.member_seen(relay, link_tx, m).await;
                }
                self.members_changed();
            }
            ServerMsg::Joined { member } => {
                self.member_seen(relay, link_tx, member).await;
                self.members_changed();
            }
            ServerMsg::Presence { id, online } => {
                if let Some(g) = self.guests.get_mut(&id) {
                    g.online = online;
                    if !online {
                        drop_link(g);
                    } else if g.link.is_none() {
                        self.offer_to(relay, link_tx, id).await;
                    }
                }
                self.members_changed();
            }
            ServerMsg::Left { id, .. } => {
                if let Some(mut g) = self.guests.remove(&id) {
                    drop_link(&mut g);
                }
                self.members_changed();
            }
            ServerMsg::Msg { from, body } => {
                let Ok(signal) = serde_json::from_value::<Signal>(body) else { return };
                let Some(link) = self.guests.get(&from).and_then(|g| g.link.clone()) else { return };
                match signal {
                    Signal::Answer { sdp } => {
                        let _ = link.set_answer(&sdp).await;
                    }
                    Signal::Candidate { json } => {
                        let _ = link.add_candidate(&json).await;
                    }
                    Signal::Offer { .. } => {}
                }
            }
            ServerMsg::Error { .. } | ServerMsg::Closed { .. } | ServerMsg::Pong => {}
        }
    }

    async fn member_seen(&mut self, relay: &Relay, link_tx: &mpsc::UnboundedSender<LinkMsg>, m: Member) {
        let g = self.guests.entry(m.id).or_insert_with(|| Guest {
            name: m.name.clone(),
            hue: m.hue,
            online: m.online,
            perms: Perms::default(),
            link: None,
            generation: 0,
            open: false,
            retries: 0,
            serving: HashMap::new(),
            window: (0, 0),
        });
        g.name = m.name;
        g.hue = m.hue;
        g.online = m.online;
        if g.online && g.link.is_none() {
            self.offer_to(relay, link_tx, m.id).await;
        }
    }

    /// Starts a direct connection to a guest: builds the link and sends the offer.
    async fn offer_to(&mut self, relay: &Relay, link_tx: &mpsc::UnboundedSender<LinkMsg>, id: MemberId) {
        let generation = self.next_generation;
        self.next_generation += 1;
        let Ok((mut link, sdp)) = Link::offer(self.cfg.link.clone()).await else { return };
        let Some(mut events) = link.take_events() else { return };
        let Some(g) = self.guests.get_mut(&id) else { return };
        g.link = Some(Arc::new(link));
        g.generation = generation;
        g.open = false;
        let tx = link_tx.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if tx.send(LinkMsg { guest: id, generation, event }).is_err() {
                    break;
                }
            }
        });
        relay_signal(relay, id, &Signal::Offer { sdp });
    }

    // ---- links ----

    async fn on_link(&mut self, relay: &Relay, link_tx: &mpsc::UnboundedSender<LinkMsg>, msg: LinkMsg) {
        let Some(g) = self.guests.get_mut(&msg.guest) else { return };
        if g.generation != msg.generation {
            return; // an event of a link that was replaced
        }
        match msg.event {
            LinkEvent::Candidate(json) => relay_signal(relay, msg.guest, &Signal::Candidate { json }),
            LinkEvent::Open => {
                g.open = true;
                g.retries = 0;
                if let Some(link) = g.link.clone() {
                    send_control(&link, &ToGuest::Perms(g.perms)).await;
                    send_control(&link, &ToGuest::State(self.current_state())).await;
                    if let Some((song, lyrics)) = &self.lyrics {
                        send_control(&link, &ToGuest::Lyrics { song: *song, lyrics: lyrics.clone() }).await;
                    }
                }
                self.members_changed();
            }
            LinkEvent::Closed => {
                drop_link(g);
                let retry = g.online && g.retries < MAX_RETRIES;
                g.retries += 1;
                self.members_changed();
                if retry {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    self.offer_to(relay, link_tx, msg.guest).await;
                }
            }
            LinkEvent::Control(bytes) => self.on_control(msg.guest, &bytes).await,
            LinkEvent::Audio(_) => {} // the host never receives audio
        }
    }

    async fn on_control(&mut self, id: MemberId, bytes: &[u8]) {
        let now = self.now_ms();
        let Ok(msg) = serde_json::from_slice::<ToHost>(bytes) else { return };
        let Some(g) = self.guests.get_mut(&id) else { return };
        let Some(link) = g.link.clone() else { return };
        // A guest that floods the channel is ignored, not served.
        let second = now / 1000;
        g.window = if g.window.0 == second { (second, g.window.1 + 1) } else { (second, 1) };
        if g.window.1 > REQUESTS_PER_SEC {
            return;
        }
        match msg {
            ToHost::Ping { sent_ms } => send_control(&link, &ToGuest::Pong { sent_ms, host_ms: now }).await,
            ToHost::Want { song, from } => {
                let Some(path) = self.songs.by_id.get(&song).cloned() else {
                    send_control(&link, &ToGuest::NoSong { song }).await;
                    return;
                };
                if let Some(old) = g.serving.remove(&song) {
                    old.abort();
                }
                let task = tokio::spawn(serve(link, song, path, from)).abort_handle();
                g.serving.insert(song, task);
            }
            ToHost::Cancel { song } => {
                if let Some(t) = g.serving.remove(&song) {
                    t.abort();
                }
            }
            ToHost::Request(action) => {
                if !action.allowed(g.perms) {
                    send_control(&link, &ToGuest::Denied { what: "the host has not allowed this".into() }).await;
                    return;
                }
                let from = g.name.clone();
                let _ = self.events.send(match action {
                    Action::Suggest { text } => HostEvent::Suggest { from, id, text },
                    action => HostEvent::Do { from, action },
                });
            }
        }
    }

    // ---- state ----

    fn current_state(&self) -> HostState {
        let mut s = self.state.clone();
        // Bring the position up to now, so a guest that joins late starts in the right place.
        if s.playing {
            let elapsed = self.now_ms().saturating_sub(s.at_ms).min(u32::MAX as u64) as u32;
            s.pos_ms = s.pos_ms.saturating_add(elapsed);
            if let Some(song) = &s.song {
                if song.duration_ms > 0 {
                    s.pos_ms = s.pos_ms.min(song.duration_ms);
                }
            }
            s.at_ms = self.now_ms();
        }
        s
    }

    async fn publish(&mut self, snapshot: Snapshot) {
        self.snapshot_at = self.now_ms();
        self.snapshot = snapshot;
        self.broadcast_state(true).await;
    }

    /// Sends the state to every guest with an open link. `changed` means the player changed;
    /// otherwise it is the heartbeat.
    async fn broadcast_state(&mut self, changed: bool) {
        if changed {
            let song = self.snapshot.current.clone().map(|s| self.songs.reference(&s));
            let queue = self.snapshot.queue.clone().iter().take(MAX_QUEUE).map(|s| self.songs.reference(s)).collect();
            self.state = HostState {
                rev: self.state.rev + 1,
                song,
                playing: self.snapshot.playing,
                pos_ms: self.snapshot.pos_ms,
                at_ms: self.snapshot_at,
                queue,
            };
        } else {
            self.state = HostState { rev: self.state.rev + 1, ..self.current_state() };
        }
        let msg = ToGuest::State(self.state.clone());
        let links: Vec<Arc<Link>> = self.guests.values().filter(|g| g.open).filter_map(|g| g.link.clone()).collect();
        for link in links {
            send_control(&link, &msg).await;
        }
    }

    fn members_changed(&self) {
        let mut members: Vec<MemberView> = self
            .guests
            .iter()
            .map(|(&id, g)| MemberView { id, name: g.name.clone(), hue: g.hue, online: g.online, linked: g.open, perms: g.perms })
            .collect();
        members.sort_by_key(|m| m.id);
        let _ = self.events.send(HostEvent::Members(members));
    }
}

fn drop_link(g: &mut Guest) {
    g.open = false;
    g.link = None;
    for (_, t) in g.serving.drain() {
        t.abort();
    }
}

fn relay_signal(relay: &Relay, to: MemberId, signal: &Signal) {
    if let Ok(body) = serde_json::to_value(signal) {
        relay.send(ClientMsg::Send { to: Target::Member(to), body });
    }
}

async fn send_control(link: &Link, msg: &ToGuest) {
    if let Ok(bytes) = serde_json::to_vec(msg) {
        let _ = link.send_control(&bytes).await;
    }
}

/// Sends a song's file to a guest, from byte `from`. A file that is still being downloaded is
/// followed until it is complete.
async fn serve(link: Arc<Link>, song: SongId, path: PathBuf, from: u64) {
    let Ok(mut file) = tokio::fs::File::open(&path).await else {
        send_control(&link, &ToGuest::NoSong { song }).await;
        return;
    };
    let growing = growth::lookup(&path);
    let mut offset = from;
    if file.seek(std::io::SeekFrom::Start(from)).await.is_err() {
        send_control(&link, &ToGuest::NoSong { song }).await;
        return;
    }
    let mut buf = vec![0u8; CHUNK - AUDIO_HEADER];
    loop {
        // How far the file may be read: all of it, or what the download has written so far.
        let readable = match &growing {
            Some(g) if !g.is_over() => Some(g.written()),
            Some(g) if g.is_failed() => {
                send_control(&link, &ToGuest::NoSong { song }).await;
                return;
            }
            _ => None,
        };
        if readable.is_some_and(|w| offset >= w) {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        }
        let want = match readable {
            Some(w) => ((w - offset) as usize).min(buf.len()),
            None => buf.len(),
        };
        let n = match file.read(&mut buf[..want]).await {
            Ok(n) => n,
            Err(_) => {
                send_control(&link, &ToGuest::NoSong { song }).await;
                return;
            }
        };
        if n == 0 {
            if readable.is_some() {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
            break;
        }
        if link.send_audio(&audio_frame(song, offset, &buf[..n])).await.is_err() {
            return; // the guest is gone
        }
        offset += n as u64;
    }
    send_control(&link, &ToGuest::AudioEnd { song, size: offset }).await;
}
