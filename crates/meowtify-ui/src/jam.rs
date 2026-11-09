//! Jam sessions in the window: start or join one, show who is in it, and keep the player in step.
//!
//! The page is in the window and the network is in `meowtify-jam-net`; this module is the one
//! place that knows both. The rest of the UI sees three calls:
//!
//! - [`install`] sets the page up,
//! - [`on_core_event`] tells it the player changed,
//! - [`filter`] lets it take over a command on its way to the player. A guest's play, pause and
//!   skip buttons become requests to the host instead.
//!
//! Everything here runs on the UI thread. Network events come in through `invoke_from_event_loop`.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use meowtify_core::{AppHandle, Command, Event, Selection, Track};
use meowtify_jam::follower::{Local, Step};
use meowtify_jam::proto::{Action, HostState, Perms, SharedLyrics, SongId, SongRef, BY_HOST, MAX_QUEUE};
use meowtify_provider::{LyricLine, Lyrics};
use meowtify_jam::{JamLink, Member, MemberId};
use meowtify_jam_net::{
    fetch_ice, Guest, GuestConfig, GuestEvent, Host, HostConfig, HostEvent, LocalCopy, MemberView, Options, RelayConfig, Snapshot, SongInfo,
};
use meowtify_library::{Library, Settings};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::{profile, JamMember, MainWindow};

/// The host's position may differ this much from where it was expected to be before guests are
/// told. Smaller differences are normal playback.
const POS_JUMP_MS: u128 = 1_500;
const MAX_SUGGESTIONS: usize = 20;
/// Longest display name this module sends. The relay accepts a few more.
const NAME_CHARS: usize = 24;

thread_local! {
    static JAM: RefCell<Option<Rc<Jam>>> = const { RefCell::new(None) };
}

fn current() -> Option<Rc<Jam>> {
    JAM.with(|j| j.borrow().clone())
}

/// Whether this window is a guest in a jam right now.
pub(crate) fn is_guest() -> bool {
    JAM.with(|j| j.borrow().as_ref().is_some_and(|j| j.session.try_borrow().is_ok_and(|s| matches!(*s, Session::Guest(_)))))
}

/// A guest's suggestion put the song at `uri` in the queue. The queue shows who asked.
pub(crate) fn note_adder(uri: &str, by: u32) {
    with(|j| {
        let mut p = j.player.borrow_mut();
        if p.adders.len() >= 64 {
            p.adders.clear();
        }
        p.adders.insert(uri.to_string(), by);
    });
}

/// The colour of the bar beside a queued song of the host's queue panel: the cat colour of whoever
/// queued it. `None` outside a hosted jam and for songs people did not queue.
pub(crate) fn queue_mark(uri: &str, queued_by_people: bool) -> Option<slint::Color> {
    if !queued_by_people {
        return None;
    }
    let j = current()?;
    let session = j.session.try_borrow().ok()?;
    let Session::Host(h) = &*session else { return None };
    let by = j.player.try_borrow().ok()?.adders.get(uri).copied();
    Some(match by.and_then(|id| h.guests.iter().find(|g| g.id == id)) {
        Some(g) => profile::color(&g.name, g.hue),
        None => profile::color(&h.name, hue_of_setting(j.win.upgrade().map_or(-1, |w| w.get_jam_hue()))),
    })
}

/// Lets go of what the module holds, at the end of the program. Dropping the handle to the
/// player lets the background tasks end, so the process can exit.
pub(crate) fn clear() {
    JAM.with(|j| *j.borrow_mut() = None);
    meowtify_core::set_cover_id("", None);
}

/// Sets the Jam page up. `links` carries `meowtify://jam/…` links that were opened from outside
/// the app. Without a library (mock mode) the page says jams are unavailable.
pub(crate) fn install(
    win: &MainWindow,
    handle: AppHandle,
    library: Option<Arc<Library>>,
    rt: tokio::runtime::Handle,
    toast: Rc<dyn Fn(&str)>,
    links: Option<UnboundedReceiver<String>>,
    mut settings: Settings,
) {
    let cache_dir = std::env::temp_dir().join("meowtify-jam");
    // A crash or a power cut leaves the last jam's songs behind.
    meowtify_jam_net::guest::remove_cache(&cache_dir);

    win.set_jam_relay(settings.jam_relay.as_str().into());
    // The first start hands out a name, and from then on it is the user's to change.
    if settings.jam_name.trim().is_empty() {
        if let Some(name) = profile::random_name() {
            settings.jam_name = name.clone();
            let _ = Settings::update(|s| s.jam_name = name);
        }
    }
    win.set_jam_name(settings.jam_name.as_str().into());
    win.set_jam_palette(ModelRc::from(Rc::new(VecModel::from(
        profile::PALETTE.map(|hue| crate::JamSwatch { hue: i32::from(hue), color: profile::swatch(hue) }).to_vec(),
    ))));
    win.set_jam_share_ip(settings.jam_share_ip);
    win.set_jam_hue(settings.jam_hue.map_or(-1, i32::from));
    win.set_my_avatar(profile::avatar(&settings.jam_name, settings.jam_hue));
    let members = Rc::new(VecModel::<JamMember>::default());
    let suggestions = Rc::new(VecModel::<SharedString>::default());
    win.set_jam_members(ModelRc::from(members.clone()));
    win.set_jam_suggestions(ModelRc::from(suggestions.clone()));

    let jam = Rc::new(Jam {
        win: win.as_weak(),
        handle,
        library,
        rt: rt.clone(),
        toast,
        members,
        suggestions,
        suggestion_texts: RefCell::new(Vec::new()),
        session: RefCell::new(Session::Idle),
        player: RefCell::new(Player::default()),
        generation: Cell::new(0),
        cache_dir,
        invite: RefCell::new(None),
        shown_queue: RefCell::new(None),
        shared_for: RefCell::new(None),
    });
    JAM.with(|j| *j.borrow_mut() = Some(jam));

    win.on_jam_host(|| with(|j| j.start_host()));
    win.on_jam_join(|text| with(|j| j.join_text(text.as_str())));
    win.on_jam_leave(|| with(|j| j.leave()));
    win.on_jam_copy(|| with(|j| j.copy_link()));
    win.on_jam_kick(|id| with(|j| j.kick(id)));
    win.on_jam_perm(|id, kind, on| with(|j| j.set_perm(id, kind, on)));
    win.on_jam_suggest(|text| with(|j| j.suggest(text.as_str())));
    win.on_jam_set_relay(|text| with(|j| j.save_relay(text.as_str())));
    win.on_jam_set_name(|text| with(|j| j.save_name(text.as_str())));
    win.on_jam_set_share_ip(|on| with(|j| j.save_share_ip(on)));
    win.on_jam_set_hue(|hue| with(|j| j.save_hue(hue)));
    win.on_jam_shuffle(|| with(|j| j.save_name("")));
    win.on_jam_accept_invite(|| with(|j| j.accept_invite()));
    win.on_jam_dismiss_invite(|| with(|j| j.dismiss_invite()));
    win.on_jam_search(|idx| with(|j| j.search_suggestion(idx)));
    win.on_jam_add(|idx| with(|j| j.add_suggestion(idx)));

    if let Some(mut links) = links {
        rt.spawn(async move {
            while let Some(link) = links.recv().await {
                if slint::invoke_from_event_loop(move || with(|j| j.invite(&link))).is_err() {
                    break;
                }
            }
        });
    }
}

/// The player changed. A host tells its guests; a guest tells its follower where its player is.
pub(crate) fn on_core_event(event: &Event) {
    if let Some(j) = current() {
        j.core_event(event);
    }
}

/// Takes a command meant for the player. `None` means the jam dealt with it: a guest's
/// transport buttons ask the host, and a guest cannot start songs of its own.
pub(crate) fn filter(cmd: Command) -> Option<Command> {
    match current() {
        Some(j) => j.filter(cmd),
        None => Some(cmd),
    }
}

fn with(f: impl FnOnce(&Jam)) {
    if let Some(j) = current() {
        f(&j);
    }
}

/// The player as the last core events described it.
#[derive(Default)]
struct Player {
    track: Option<Track>,
    playing: bool,
    pos_ms: u32,
    queue: Vec<Track>,
    /// The `Queue::rev` that `queue` was made from.
    queue_rev: u64,
    /// How many songs at the front of `queue` people put there, the rest being autoplay's.
    manual: usize,
    /// Who queued a song, by file, for songs a guest's suggestion put in the queue. Others were
    /// queued by the host.
    adders: std::collections::HashMap<String, u32>,
}

/// The relay did not answer the request for its TURN server, so hiding the address is not possible.
fn no_reach(why: &str) -> String {
    format!("Could not ask the relay for its TURN server ({why}). Check the connection or the proxy in Settings, or turn on Share my IP to connect directly.")
}

const NO_TURN: &str = "This relay has no TURN server, so it cannot hide your IP address. Turn on Share my IP to connect directly, or use another relay.";

/// The hue the window holds: 0 to 359, or negative for none.
fn hue_of_setting(hue: i32) -> Option<u16> {
    u16::try_from(hue).ok().filter(|h| *h < 360)
}

enum Session {
    Idle,
    /// Asking the relay for network settings; nothing is open yet.
    Starting,
    Host(HostSession),
    Guest(GuestSession),
}

struct HostSession {
    host: Host,
    gate: Gate,
    link: String,
    name: String,
    guests: Vec<MemberView>,
}

struct GuestSession {
    guest: Guest,
    perms: Perms,
    /// The song the host is on, for the title of the file when it is played.
    song: Option<SongRef>,
    members: Vec<Member>,
    /// The words the host sent last, with the song they belong to.
    lyrics: Option<(SongId, SharedLyrics)>,
    /// The song that plays here now and its file.
    now: Option<(SongId, String)>,
}

struct Jam {
    win: Weak<MainWindow>,
    handle: AppHandle,
    library: Option<Arc<Library>>,
    rt: tokio::runtime::Handle,
    toast: Rc<dyn Fn(&str)>,
    members: Rc<VecModel<JamMember>>,
    suggestions: Rc<VecModel<SharedString>>,
    /// What each suggestion asked for, without the name of who asked.
    suggestion_texts: RefCell<Vec<(String, MemberId)>>,
    session: RefCell<Session>,
    player: RefCell<Player>,
    /// Counts sessions, so a late event from an earlier one is ignored.
    generation: Cell<u64>,
    cache_dir: PathBuf,
    invite: RefCell<Option<JamLink>>,
    /// The queue the guest panel shows now, to redraw it only when it changes.
    shown_queue: RefCell<Option<Vec<crate::JamQueueRow>>>,
    /// The file whose lyrics the host side looked up last, so each song is looked up once.
    shared_for: RefCell<Option<Arc<str>>>,
}

impl Jam {
    fn send(&self, cmd: Command) {
        let handle = self.handle.clone();
        self.rt.spawn(async move {
            let _ = handle.send(cmd).await;
        });
    }

    fn say(&self, text: &str) {
        (self.toast)(text);
    }

    fn status(&self, text: &str) {
        if let Some(w) = self.win.upgrade() {
            w.set_jam_status(text.into());
        }
    }

    fn idle(&self) -> bool {
        matches!(*self.session.borrow(), Session::Idle)
    }

    // --- Starting and ending -------------------------------------------------------------

    fn start_host(&self) {
        let Some(w) = self.win.upgrade() else { return };
        if self.library.is_none() {
            return self.status("Jams need the real player, not the mock one.");
        }
        if !self.idle() {
            return;
        }
        let relay = w.get_jam_relay().trim().to_string();
        if !is_relay_address(&relay) {
            return self.status("Enter the relay address first, such as wss://jam.example.com");
        }
        self.save_relay(&relay);
        let (Some(code), Some(key)) = (random_code(10), random_code(32)) else {
            return self.status("The system gave no random numbers, so the jam cannot be made safe.");
        };
        let name = display_name(w.get_jam_name().as_str());
        let hue = hue_of_setting(w.get_jam_hue());
        let hide_ip = !w.get_jam_share_ip();
        let link = JamLink { code: code.clone(), relay: relay.clone() }.to_url();
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        *self.session.borrow_mut() = Session::Starting;
        self.status("Opening the room…");

        let cfg_name = name.clone();
        self.rt.spawn(async move {
            let ice = match fetch_ice(&relay, hide_ip, meowtify_provider::proxy().as_deref()).await {
                Ok(ice) if ice.usable() => ice,
                Ok(_) => {
                    let _ = slint::invoke_from_event_loop(move || with(|j| j.blocked(generation, NO_TURN)));
                    return;
                }
                Err(why) => {
                    let _ = slint::invoke_from_event_loop(move || with(|j| j.blocked(generation, &no_reach(&why))));
                    return;
                }
            };
            let (host, mut events) = Host::start(HostConfig {
                relay: RelayConfig { url: relay, code, name: cfg_name, hue, host_key: Some(key) },
                link: Options { ice, bind: Vec::new() },
            });
            let delivered = slint::invoke_from_event_loop(move || with(|j| j.host_ready(generation, host, link, name)));
            if delivered.is_err() {
                return;
            }
            while let Some(event) = events.recv().await {
                if slint::invoke_from_event_loop(move || with(|j| j.host_event(generation, event))).is_err() {
                    break;
                }
            }
        });
    }

    fn host_ready(&self, generation: u64, host: Host, link: String, name: String) {
        if generation != self.generation.get() || !matches!(*self.session.borrow(), Session::Starting) {
            return; // the user gave up while the room was opening; dropping the host closes it
        }
        let mut gate = Gate::default();
        // A guest who joins now must find out what plays.
        let snapshot = snapshot_of(&self.player.borrow());
        gate.changed(&snapshot, Instant::now());
        host.publish(snapshot);
        *self.session.borrow_mut() = Session::Host(HostSession { host, gate, link: link.clone(), name, guests: Vec::new() });
        if let Some(w) = self.win.upgrade() {
            w.set_jam_mode(1);
            w.set_jam_link(link.as_str().into());
            w.set_jam_join_text(SharedString::default());
        }
        self.status("Hosting. Anyone with the link can join.");
        self.show_members();
    }

    fn join_text(&self, text: &str) {
        match JamLink::parse(text) {
            Some(link) => self.start_join(link),
            None => self.status("That is not a Meowtify jam link."),
        }
    }

    fn start_join(&self, link: JamLink) {
        let Some(w) = self.win.upgrade() else { return };
        if self.library.is_none() {
            return self.status("Jams need the real player, not the mock one.");
        }
        if !self.idle() {
            return self.say("Leave your current jam first");
        }
        let name = display_name(w.get_jam_name().as_str());
        let hue = hue_of_setting(w.get_jam_hue());
        let hide_ip = !w.get_jam_share_ip();
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        *self.session.borrow_mut() = Session::Starting;
        w.set_jam_join_text(SharedString::default());
        self.status("Joining…");

        let cache_dir = self.cache_dir.clone();
        let local_copy = self.library.clone().map(|lib| own_copy_finder(lib, cache_dir.clone()));
        self.rt.spawn(async move {
            let ice = match fetch_ice(&link.relay, hide_ip, meowtify_provider::proxy().as_deref()).await {
                Ok(ice) if ice.usable() => ice,
                Ok(_) => {
                    let _ = slint::invoke_from_event_loop(move || with(|j| j.blocked(generation, NO_TURN)));
                    return;
                }
                Err(why) => {
                    let _ = slint::invoke_from_event_loop(move || with(|j| j.blocked(generation, &no_reach(&why))));
                    return;
                }
            };
            let (guest, mut events) = Guest::join(GuestConfig {
                relay: RelayConfig { url: link.relay, code: link.code, name, hue, host_key: None },
                link: Options { ice, bind: Vec::new() },
                cache_dir,
                local_copy,
            });
            if slint::invoke_from_event_loop(move || with(|j| j.guest_ready(generation, guest))).is_err() {
                return;
            }
            while let Some(event) = events.recv().await {
                if slint::invoke_from_event_loop(move || with(|j| j.guest_event(generation, event))).is_err() {
                    break;
                }
            }
        });
    }

    fn guest_ready(&self, generation: u64, guest: Guest) {
        if generation != self.generation.get() || !matches!(*self.session.borrow(), Session::Starting) {
            return;
        }
        *self.session.borrow_mut() = Session::Guest(GuestSession { guest, perms: Perms::LISTENER, song: None, members: Vec::new(), lyrics: None, now: None });
        // The host decides what plays. Own songs and autoplay would only fight it.
        crate::AUTOPLAY.with(|a| {
            if let Some(c) = a.borrow().as_ref() {
                c.set_enabled(false);
            }
        });
        self.send(Command::Pause);
        if let Some(w) = self.win.upgrade() {
            w.set_jam_mode(2);
            w.set_jam_link(SharedString::default());
            w.set_jam_rights(rights_line(Perms::LISTENER).into());
        }
        self.status("Connecting to the host…");
    }

    /// The jam cannot start the way the user asked. Goes back to idle and says why.
    fn blocked(&self, generation: u64, why: &str) {
        if generation == self.generation.get() && matches!(*self.session.borrow(), Session::Starting) {
            *self.session.borrow_mut() = Session::Idle;
            self.status(why);
        }
    }

    /// Keeps the choice to connect directly, which shows our IP address to the other people.
    fn save_share_ip(&self, on: bool) {
        if let Err(e) = Settings::update(|s| s.jam_share_ip = on) {
            self.status(&format!("Could not save settings: {e}"));
        }
        if let Some(w) = self.win.upgrade() {
            w.set_jam_share_ip(on);
        }
    }

    /// Ends the session, whoever ended it. `why` is shown to the user.
    fn finish(&self, why: &str) {
        let was_guest = matches!(*self.session.borrow(), Session::Guest(_));
        // Dropping the session closes the network side and clears a guest's downloaded songs.
        *self.session.borrow_mut() = Session::Idle;
        self.generation.set(self.generation.get() + 1);
        self.members.set_vec(Vec::new());
        self.suggestions.set_vec(Vec::new());
        self.suggestion_texts.borrow_mut().clear();
        if was_guest {
            self.send(Command::Pause);
            let on = Settings::load().autoplay;
            crate::AUTOPLAY.with(|a| {
                if let Some(c) = a.borrow().as_ref() {
                    c.set_enabled(on);
                }
            });
        }
        meowtify_core::set_cover_id("", None);
        *self.shown_queue.borrow_mut() = None;
        *self.shared_for.borrow_mut() = None;
        if let Some(w) = self.win.upgrade() {
            w.set_jam_mode(0);
            w.set_jam_link(SharedString::default());
            w.set_jam_rights(SharedString::default());
            if was_guest {
                crate::show_jam_queue(&w, &[]);
                crate::lyrics::offer(&w, None);
            }
        }
        self.status(why);
        self.say(why);
    }

    fn leave(&self) {
        let (hosting, guest) = match &*self.session.borrow() {
            Session::Host(h) => {
                h.host.end();
                (true, false)
            }
            Session::Guest(g) => {
                g.guest.leave();
                (false, true)
            }
            _ => (false, false),
        };
        if hosting || guest || matches!(*self.session.borrow(), Session::Starting) {
            self.finish(if hosting { "The jam is over" } else { "You left the jam" });
        }
    }

    // --- The host side -------------------------------------------------------------------

    fn host_event(&self, generation: u64, event: HostEvent) {
        if generation != self.generation.get() {
            return;
        }
        match event {
            HostEvent::Members(guests) => {
                if let Session::Host(h) = &mut *self.session.borrow_mut() {
                    h.guests = guests;
                }
                self.show_members();
            }
            HostEvent::Do { from, action } => {
                let (cmd, what) = match action {
                    Action::Play => (Command::Play, "resumed".to_string()),
                    Action::Pause => (Command::Pause, "paused".to_string()),
                    Action::Seek { ms } => (Command::Seek(ms), "moved the song".to_string()),
                    Action::Next => (Command::Next, "skipped".to_string()),
                    Action::Previous => (Command::Previous, "went back".to_string()),
                    Action::Suggest { .. } => return,
                };
                self.say(&format!("{from} {what}"));
                self.send(cmd);
            }
            HostEvent::Suggest { from, id, text } => {
                self.say(&format!("{from} suggests {text}"));
                let mut texts = self.suggestion_texts.borrow_mut();
                if texts.len() >= MAX_SUGGESTIONS {
                    texts.remove(0);
                    self.suggestions.remove(0);
                }
                texts.push((text.clone(), id));
                self.suggestions.push(format!("{from}: {text}").into());
            }
            HostEvent::Reconnecting(why) => self.status(&format!("Lost the relay ({why}). Trying again…")),
            HostEvent::Down(why) => self.finish(&format!("The jam closed: {why}")),
        }
    }

    fn show_members(&self) {
        let mut rows = Vec::new();
        match &*self.session.borrow() {
            Session::Host(h) => {
                rows.push(JamMember {
                    id: -1,
                    name: h.name.as_str().into(),
                    avatar: profile::avatar(&h.name, hue_of_setting(self.win.upgrade().map_or(-1, |w| w.get_jam_hue()))),
                    state: "you".into(),
                    host: true,
                    control: true,
                    skip: true,
                    add: true,
                });
                for g in h.guests.iter().filter(|g| !is_ghost(&g.name, g.online, h.guests.iter().map(|o| (o.name.as_str(), o.online)))) {
                    rows.push(JamMember {
                        id: g.id as i32,
                        name: g.name.as_str().into(),
                        avatar: profile::avatar(&g.name, g.hue),
                        state: match (g.online, g.linked) {
                            (_, true) => "connected",
                            (true, false) => "connecting…",
                            (false, false) => "offline",
                        }
                        .into(),
                        host: false,
                        control: g.perms.control,
                        skip: g.perms.skip,
                        add: g.perms.add,
                    });
                }
            }
            Session::Guest(g) => {
                for m in g.members.iter().filter(|m| !is_ghost(&m.name, m.online, g.members.iter().map(|o| (o.name.as_str(), o.online)))) {
                    rows.push(JamMember {
                        id: m.id as i32,
                        name: m.name.as_str().into(),
                        avatar: profile::avatar(&m.name, m.hue),
                        state: if m.online { "online" } else { "offline" }.into(),
                        host: m.host,
                        control: false,
                        skip: false,
                        add: false,
                    });
                }
            }
            _ => {}
        }
        self.members.set_vec(rows);
    }

    fn kick(&self, id: i32) {
        if let (Session::Host(h), Ok(id)) = (&*self.session.borrow(), MemberId::try_from(id)) {
            h.host.kick(id);
        }
    }

    fn set_perm(&self, id: i32, kind: i32, on: bool) {
        let Session::Host(h) = &*self.session.borrow() else { return };
        let Ok(id) = MemberId::try_from(id) else { return };
        let Some(guest) = h.guests.iter().find(|g| g.id == id) else { return };
        let mut perms = guest.perms;
        match kind {
            0 => perms.control = on,
            1 => perms.skip = on,
            _ => perms.add = on,
        }
        h.host.set_perms(id, perms);
    }

    /// Takes suggestion `idx` off the list and gives its text and who made it.
    fn take_suggestion(&self, idx: i32) -> Option<(String, MemberId)> {
        let idx = usize::try_from(idx).ok()?;
        let text = {
            let mut texts = self.suggestion_texts.borrow_mut();
            if idx >= texts.len() {
                return None;
            }
            texts.remove(idx)
        };
        self.suggestions.remove(idx);
        Some(text)
    }

    /// The host accepts a suggestion: the best match goes to the queue.
    fn add_suggestion(&self, idx: i32) {
        let Some((text, by)) = self.take_suggestion(idx) else { return };
        self.say("Looking for the suggested song\u{2026}");
        crate::get::GET.with(|g| {
            if let Some(g) = g.borrow().as_ref() {
                g.queue_text(&text, by);
            }
        });
    }

    /// The words of the song that plays go to the guests, once per song.
    fn share_lyrics(&self) {
        let track = match (&*self.session.borrow(), &self.player.borrow().track) {
            (Session::Host(_), Some(t)) => t.clone(),
            _ => return,
        };
        if self.shared_for.borrow().as_deref() == Some(&*track.uri) {
            return;
        }
        *self.shared_for.borrow_mut() = Some(track.uri.clone());
        let generation = self.generation.get();
        self.rt.spawn(async move {
            let artist = track.artists.first().map(|a| a.name.to_string()).unwrap_or_default();
            let album = track.album.as_ref().map(|a| a.name.to_string());
            let file = PathBuf::from(track.uri.as_ref());
            let query = meowtify_provider::LyricsQuery {
                title: &track.name,
                artist: &artist,
                album: album.as_deref(),
                duration_secs: track.duration_ms / 1000,
                audio: file.is_absolute().then_some(file.as_path()),
            };
            let cache = meowtify_library::config_dir().join("lyrics");
            let Ok(Some(found)) = meowtify_provider::find_lyrics(&query, &cache).await else { return };
            let wire = to_shared(&found);
            let _ = slint::invoke_from_event_loop(move || with(|j| j.send_lyrics(generation, file, wire)));
        });
    }

    fn send_lyrics(&self, generation: u64, path: PathBuf, lyrics: SharedLyrics) {
        if generation != self.generation.get() {
            return;
        }
        if let Session::Host(h) = &*self.session.borrow() {
            h.host.share_lyrics(path, lyrics);
        }
    }

    fn search_suggestion(&self, idx: i32) {
        let Some((text, _)) = self.take_suggestion(idx) else { return };
        if let Some(w) = self.win.upgrade() {
            w.set_view(0);
        }
        crate::get::GET.with(|g| {
            if let Some(g) = g.borrow().as_ref() {
                g.search_now(&text);
            }
        });
    }

    fn copy_link(&self) {
        let link = match &*self.session.borrow() {
            Session::Host(h) => h.link.clone(),
            _ => return,
        };
        if copy_to_clipboard(&link) {
            self.say("Link copied");
        } else {
            self.say("Could not copy. Select the link and press Ctrl+C");
        }
    }

    // --- The guest side ------------------------------------------------------------------

    fn guest_event(&self, generation: u64, event: GuestEvent) {
        if generation != self.generation.get() {
            return;
        }
        match event {
            GuestEvent::Members(members) => {
                if let Session::Guest(g) = &mut *self.session.borrow_mut() {
                    g.members = members;
                }
                self.show_members();
            }
            GuestEvent::Linked(true) => self.status("Connected. You hear what the host hears."),
            GuestEvent::Linked(false) => self.status("Connecting to the host…"),
            GuestEvent::State(state) => self.guest_state(state),
            GuestEvent::Perms(perms) => {
                if let Session::Guest(g) = &mut *self.session.borrow_mut() {
                    g.perms = perms;
                }
                if let Some(w) = self.win.upgrade() {
                    w.set_jam_rights(rights_line(perms).into());
                }
            }
            GuestEvent::Do(step) => self.apply_step(step),
            GuestEvent::Denied(what) if what.starts_with("you ") => self.say(&what),
            GuestEvent::Denied(what) => self.say(&format!("The host said no: {what}")),
            GuestEvent::Lyrics { song, lyrics } => self.guest_lyrics(song, lyrics),
            GuestEvent::Reconnecting(why) => self.status(&format!("Lost the relay ({why}). Trying again…")),
            GuestEvent::Down(why) => self.finish(&format!("The jam closed: {why}")),
        }
    }

    fn guest_state(&self, state: HostState) {
        if let Session::Guest(g) = &mut *self.session.borrow_mut() {
            g.song = state.song.clone();
        }
        // The queue panel shows what the host will play next.
        if let Some(w) = self.win.upgrade() {
            let rows: Vec<crate::JamQueueRow> = {
                let session = self.session.borrow();
                let members: &[Member] = match &*session {
                    Session::Guest(g) => &g.members,
                    _ => &[],
                };
                state
                    .queue
                    .iter()
                    .map(|s| crate::JamQueueRow {
                        title: s.title.clone(),
                        artist: s.artist.clone(),
                        key: s.key.clone(),
                        mark: s.by.and_then(|by| {
                            let who = if by == BY_HOST { members.iter().find(|m| m.host) } else { members.iter().find(|m| m.id == by) };
                            who.map(|m| profile::color(&m.name, m.hue))
                        }),
                    })
                    .collect()
            };
            if self.shown_queue.borrow().as_ref() != Some(&rows) {
                crate::show_jam_queue(&w, &rows);
                *self.shown_queue.borrow_mut() = Some(rows);
            }
        }
    }

    /// The host sent the words of a song. They show when that song plays here.
    fn guest_lyrics(&self, song: SongId, lyrics: SharedLyrics) {
        let path = {
            let mut session = self.session.borrow_mut();
            let Session::Guest(g) = &mut *session else { return };
            let path = g.now.as_ref().filter(|(id, _)| *id == song).map(|(_, p)| p.clone());
            g.lyrics = Some((song, lyrics.clone()));
            path
        };
        if let (Some(path), Some(w)) = (path, self.win.upgrade()) {
            crate::lyrics::offer(&w, Some((&path, from_shared(lyrics))));
        }
    }

    fn apply_step(&self, step: Step) {
        match step {
            // The guest crate handles downloads itself; it only reports what to play.
            Step::Want(_) => {}
            Step::Play { song, path, pos_ms, playing } => {
                let (info, shared) = {
                    let mut session = self.session.borrow_mut();
                    match &mut *session {
                        Session::Guest(g) => {
                            g.now = Some((song, path.clone()));
                            (g.song.clone().filter(|s| s.id == song), g.lyrics.clone().filter(|(id, _)| *id == song).map(|(_, l)| l))
                        }
                        _ => (None, None),
                    }
                };
                let (title, artist, duration) = match &info {
                    Some(s) => (s.title.as_str(), s.artist.as_str(), s.duration_ms),
                    None => ("Jam song", "", 0),
                };
                if let Some(library) = &self.library {
                    library.add_transient(Path::new(&path), title, artist, duration);
                }
                // The file has no id in its name; the cover comes from the one the host sent. The
                // player's window and Discord presence both look it up.
                meowtify_core::set_cover_id(&path, info.as_ref().and_then(|s| s.key.as_deref()));
                if let Some(w) = self.win.upgrade() {
                    crate::lyrics::offer(&w, shared.map(|l| (path.as_str(), from_shared(l))));
                }
                self.send(Command::PlayFrom(Selection::Single(path.as_str().into())));
                if pos_ms > 1_000 {
                    self.send(Command::Seek(pos_ms));
                }
                if !playing {
                    self.send(Command::Pause);
                }
            }
            Step::Seek(ms) => self.send(Command::Seek(ms)),
            Step::Pause | Step::Stop => self.send(Command::Pause),
            Step::Resume => self.send(Command::Play),
        }
    }

    fn suggest(&self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        if let Session::Guest(g) = &*self.session.borrow() {
            let action = Action::Suggest { text: text.to_string() };
            if action.allowed(g.perms) {
                g.guest.request(action);
                self.say("Suggestion sent");
            } else {
                self.say("The host has not allowed suggestions");
            }
        }
    }

    fn filter(&self, cmd: Command) -> Option<Command> {
        let session = self.session.borrow();
        let Session::Guest(g) = &*session else { return Some(cmd) };
        let action = match &cmd {
            Command::Play => Action::Play,
            Command::Pause => Action::Pause,
            Command::Seek(ms) => Action::Seek { ms: *ms },
            Command::Next => Action::Next,
            Command::Previous => Action::Previous,
            Command::PlayFrom(_) | Command::Enqueue { .. } | Command::Jump(_) => {
                self.say("You are in a jam. Suggest the song to the host instead");
                return None;
            }
            _ => return Some(cmd),
        };
        if action.allowed(g.perms) {
            g.guest.request(action);
        } else {
            self.say("The host has not given you that right");
        }
        None
    }

    // --- The player ----------------------------------------------------------------------

    fn core_event(&self, event: &Event) {
        {
            let mut p = self.player.borrow_mut();
            match event {
                Event::PlaybackChanged(Some(s)) => {
                    p.track = s.track.clone();
                    p.playing = s.is_playing && !s.ended;
                    p.pos_ms = s.progress_ms;
                }
                Event::PlaybackChanged(None) => {
                    p.track = None;
                    p.playing = false;
                    p.pos_ms = 0;
                }
                Event::QueueChanged(q) => {
                    if q.rev != 0 && q.rev == p.queue_rev {
                        return; // the same queue as the last time
                    }
                    p.queue_rev = q.rev;
                    p.queue = q.upcoming.iter().take(MAX_QUEUE).cloned().collect();
                    p.manual = q.manual.min(p.queue.len());
                }
                _ => return,
            }
        }
        if matches!(event, Event::PlaybackChanged(Some(_))) {
            self.share_lyrics();
        }
        let player = self.player.borrow();
        match &mut *self.session.borrow_mut() {
            Session::Host(h) => {
                let snapshot = snapshot_of(&player);
                if h.gate.changed(&snapshot, Instant::now()) {
                    h.host.publish(snapshot);
                }
            }
            Session::Guest(g) => g.guest.report(Local {
                path: player.track.as_ref().map(|t| t.uri.to_string()),
                pos_ms: player.pos_ms,
                playing: player.playing,
            }),
            _ => {}
        }
    }

    // --- Settings and links --------------------------------------------------------------

    fn save_relay(&self, text: &str) {
        let text = text.trim();
        if !text.is_empty() && !is_relay_address(text) {
            return self.status("The relay address starts with ws:// or wss://");
        }
        if let Err(e) = Settings::update(|s| {
            if s.jam_relay != text {
                s.jam_relay = text.to_string();
            }
        }) {
            self.status(&format!("Could not save settings: {e}"));
        }
    }

    /// Keeps the name, or hands out a new random one when `text` is empty. The cat follows the
    /// name. A jam that is already open keeps the name it was joined with.
    fn save_name(&self, text: &str) {
        let name: String = text.trim().chars().filter(|c| !c.is_control()).take(NAME_CHARS).collect();
        let name = if name.is_empty() { profile::random_name().unwrap_or_else(|| "Guest".to_string()) } else { name };
        if let Err(e) = Settings::update(|s| s.jam_name = name.clone()) {
            self.status(&format!("Could not save settings: {e}"));
        }
        if let Some(w) = self.win.upgrade() {
            w.set_jam_name(name.as_str().into());
            w.set_my_avatar(profile::avatar(&name, hue_of_setting(w.get_jam_hue())));
        }
    }

    /// Keeps the avatar colour: a hue in degrees, or a negative number for the colour the name gives.
    fn save_hue(&self, hue: i32) {
        let hue = hue_of_setting(hue);
        if let Err(e) = Settings::update(|s| s.jam_hue = hue) {
            self.status(&format!("Could not save settings: {e}"));
        }
        if let Some(w) = self.win.upgrade() {
            w.set_jam_hue(hue.map_or(-1, i32::from));
            w.set_my_avatar(profile::avatar(w.get_jam_name().as_str(), hue));
        }
    }

    /// A link opened from outside the app. Joining a stranger's jam needs a click.
    fn invite(&self, text: &str) {
        let Some(link) = JamLink::parse(text) else { return };
        if !self.idle() {
            return self.say("A jam link arrived. Leave your jam first to join it");
        }
        let server = link.relay.split("://").nth(1).unwrap_or(&link.relay).to_string();
        if let Some(w) = self.win.upgrade() {
            w.set_jam_invite(format!("Someone invited you to a jam on {server}. Songs play from their computer, over a direct connection. Join?").into());
            w.set_view(4);
        }
        *self.invite.borrow_mut() = Some(link);
    }

    fn accept_invite(&self) {
        let link = self.invite.borrow_mut().take();
        if let Some(w) = self.win.upgrade() {
            w.set_jam_invite(SharedString::default());
        }
        if let Some(link) = link {
            self.start_join(link);
        }
    }

    fn dismiss_invite(&self) {
        *self.invite.borrow_mut() = None;
        if let Some(w) = self.win.upgrade() {
            w.set_jam_invite(SharedString::default());
        }
    }
}

// --- Pure helpers, with tests --------------------------------------------------------------

/// Tells when the host's guests need to hear about the player: when the song, the play state
/// or the queue changed, or when the position jumped (a seek). Ordinary playback only moves the
/// position forward, which guests work out for themselves.
#[derive(Default)]
struct Gate {
    last: Option<(Snapshot, Instant)>,
}

impl Gate {
    fn changed(&mut self, now: &Snapshot, at: Instant) -> bool {
        if let Some((last, last_at)) = &self.last {
            let moved = if last.playing { at.saturating_duration_since(*last_at).as_millis() } else { 0 };
            let expected = u128::from(last.pos_ms) + moved;
            let same = last.current == now.current && last.playing == now.playing && last.queue == now.queue;
            if same && expected.abs_diff(u128::from(now.pos_ms)) <= POS_JUMP_MS {
                return false;
            }
        }
        self.last = Some((now.clone(), at));
        true
    }
}

/// Whether an offline member is only the old copy of someone who is online again under the same
/// name. A relay without the fix keeps such a copy until its grace time ends; it is not shown.
fn is_ghost<'a>(name: &str, online: bool, all: impl Iterator<Item = (&'a str, bool)>) -> bool {
    !online && all.into_iter().any(|(n, on)| on && n.trim().eq_ignore_ascii_case(name.trim()))
}

/// Lyrics as they go over the wire.
fn to_shared(l: &Lyrics) -> SharedLyrics {
    match l {
        Lyrics::Synced(lines) => SharedLyrics { synced: lines.iter().map(|l| (l.at_ms, l.text.clone())).collect(), ..Default::default() },
        Lyrics::Plain(lines) => SharedLyrics { plain: lines.clone(), ..Default::default() },
        Lyrics::Instrumental => SharedLyrics { instrumental: true, ..Default::default() },
    }
}

/// Lyrics from the wire, as the lyrics panel holds them.
fn from_shared(l: SharedLyrics) -> Lyrics {
    if l.instrumental {
        Lyrics::Instrumental
    } else if !l.synced.is_empty() {
        Lyrics::Synced(l.synced.into_iter().map(|(at_ms, text)| LyricLine { at_ms, text }).collect())
    } else {
        Lyrics::Plain(l.plain)
    }
}

fn snapshot_of(p: &Player) -> Snapshot {
    Snapshot {
        current: p.track.as_ref().map(|t| song_info(t, None)),
        playing: p.playing,
        pos_ms: p.pos_ms,
        queue: p
            .queue
            .iter()
            .enumerate()
            .map(|(i, t)| song_info(t, (i < p.manual).then(|| p.adders.get(t.uri.as_ref()).copied().unwrap_or(BY_HOST))))
            .collect(),
    }
}

fn song_info(t: &Track, by: Option<u32>) -> SongInfo {
    SongInfo {
        by,
        path: PathBuf::from(t.uri.as_ref()),
        title: t.name.to_string(),
        artist: crate::artist_line(t),
        duration_ms: t.duration_ms,
        key: meowtify_core::youtube_id(t.uri.as_ref()).map(str::to_string),
    }
}

/// How far a song's length may differ from the host's before it counts as another recording.
const SAME_SONG_SLACK_MS: u32 = 2_000;

/// Finds a song in the guest's own library by the id the host sent, so the song is not sent. The
/// file must be a whole one that is not part of the jam's cache, and its length must match the
/// host's, because a different cut with the same id would drift out of time.
fn own_copy_finder(library: Arc<Library>, cache_dir: PathBuf) -> LocalCopy {
    LocalCopy(Arc::new(move |song: &SongRef| {
        let key = song.key.as_deref()?;
        let track = library.find_by_video_id(key)?;
        let path = PathBuf::from(track.uri.as_ref());
        let same_length = track.duration_ms == 0 || song.duration_ms == 0 || track.duration_ms.abs_diff(song.duration_ms) <= SAME_SONG_SLACK_MS;
        let whole = meowtify_core::growth::lookup(&path).is_none_or(|g| g.is_done());
        (same_length && whole && !path.starts_with(&cache_dir)).then_some(path)
    }))
}

fn is_relay_address(text: &str) -> bool {
    (text.starts_with("ws://") || text.starts_with("wss://")) && text.len() > 8 && text.chars().all(|c| c.is_ascii_graphic())
}

/// What the guest may do, in words.
fn rights_line(p: Perms) -> String {
    let mut can = Vec::new();
    if p.add {
        can.push("suggest songs");
    }
    if p.control {
        can.push("pause, resume and seek");
    }
    if p.skip {
        can.push("skip");
    }
    if can.is_empty() {
        "You can listen. The host has not given you any controls.".to_string()
    } else {
        format!("You can {}.", can.join(", "))
    }
}

/// The name others see: the chosen one, else the Windows user name.
fn display_name(chosen: &str) -> String {
    let name = chosen.trim();
    let name = if name.is_empty() { std::env::var("USERNAME").unwrap_or_default() } else { name.to_string() };
    let name: String = name.chars().filter(|c| !c.is_control()).take(NAME_CHARS).collect();
    if name.trim().is_empty() {
        "Guest".to_string()
    } else {
        name
    }
}

/// `len` random lower case letters and digits, from the system's random source.
fn random_code(len: usize) -> Option<String> {
    const ALPHABET: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut out = String::with_capacity(len);
    let mut buf = [0u8; 64];
    while out.len() < len {
        getrandom::fill(&mut buf).ok()?;
        for &b in &buf {
            // 252 = 7 * 36: bytes above it are skipped so every character is equally likely.
            if b < 252 && out.len() < len {
                out.push(ALPHABET[usize::from(b % 36)] as char);
            }
        }
    }
    Some(out)
}

#[cfg(windows)]
fn copy_to_clipboard(text: &str) -> bool {
    use std::io::Write;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    let Ok(mut child) = Command::new("clip").stdin(Stdio::piped()).creation_flags(0x0800_0000).spawn() else { return false };
    let wrote = child.stdin.take().is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
    child.wait().is_ok_and(|s| s.success()) && wrote
}

#[cfg(not(windows))]
fn copy_to_clipboard(_text: &str) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::is_ghost;

    #[test]
    fn an_offline_copy_of_someone_who_is_back_is_hidden() {
        let all = [("Ann", false), ("ann ", true), ("Bo", false)];
        assert!(is_ghost("Ann", false, all.iter().copied()));
        assert!(!is_ghost("ann ", true, all.iter().copied()), "the online one stays");
        assert!(!is_ghost("Bo", false, all.iter().copied()), "nobody online under that name");
    }

    use super::*;
    use std::time::Duration;

    fn snap(title: &str, playing: bool, pos_ms: u32) -> Snapshot {
        Snapshot {
            current: Some(SongInfo { path: format!("/{title}").into(), title: title.into(), artist: String::new(), duration_ms: 200_000, key: None, by: None }),
            playing,
            pos_ms,
            queue: Vec::new(),
        }
    }

    #[test]
    fn guests_hear_changes_and_jumps_but_not_ordinary_playback() {
        let t0 = Instant::now();
        let mut gate = Gate::default();
        assert!(gate.changed(&snap("a", true, 0), t0), "the first state is always sent");
        let later = t0 + Duration::from_secs(10);
        assert!(!gate.changed(&snap("a", true, 10_200), later), "playing on as expected");
        assert!(gate.changed(&snap("a", true, 60_000), later), "a seek");
        assert!(gate.changed(&snap("a", false, 60_100), later), "a pause");
        assert!(!gate.changed(&snap("a", false, 60_100), later + Duration::from_secs(30)), "paused stays put");
        assert!(gate.changed(&snap("b", false, 0), later), "another song");
    }

    #[test]
    fn a_changed_queue_is_news() {
        let t0 = Instant::now();
        let mut gate = Gate::default();
        let mut s = snap("a", true, 0);
        gate.changed(&s, t0);
        s.queue.push(SongInfo { path: "/q".into(), title: "q".into(), artist: String::new(), duration_ms: 1, key: None, by: None });
        assert!(gate.changed(&s, t0));
    }

    #[test]
    fn rights_are_told_in_words() {
        assert_eq!(rights_line(Perms::LISTENER), "You can suggest songs.");
        assert_eq!(rights_line(Perms::DJ), "You can suggest songs, pause, resume and seek, skip.");
        assert!(rights_line(Perms::NONE).starts_with("You can listen"));
    }

    #[test]
    fn names_are_plain_and_short() {
        assert_eq!(display_name("  Hana  "), "Hana");
        assert_eq!(display_name(&"x".repeat(100)).chars().count(), NAME_CHARS);
        assert_eq!(display_name("a\u{7}b"), "ab");
    }

    #[test]
    fn codes_are_random_letters_and_digits() {
        let a = random_code(32).unwrap();
        let b = random_code(32).unwrap();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
        assert_ne!(a, b);
    }

    #[test]
    fn only_websocket_addresses_are_relays() {
        assert!(is_relay_address("wss://jam.example.com"));
        assert!(is_relay_address("ws://127.0.0.1:8787"));
        assert!(!is_relay_address("https://jam.example.com"));
        assert!(!is_relay_address("wss://"));
        assert!(!is_relay_address("wss://a b.example"));
    }
}
