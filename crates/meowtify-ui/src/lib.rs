//! Slint front end. Browses a [`MetadataSource`] (liked songs, playlists,
//! search), sends [`Command`] values to the core, and shows [`Event`] values.
//!
//! Cover images are YouTube thumbnails (see `thumbs`). Files with no known cover
//! get a generated gradient.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use meowtify_core::{AppHandle, Command, Event, MetadataSource, Playlist, Repeat, Track};
use meowtify_library::{Library, Settings};
use meowtify_provider::DownloadManager;
use slint::{
    Image, Model, ModelNotify, ModelRc, ModelTracker, Rgba8Pixel, SharedPixelBuffer, SharedString,
    VecModel,
};
use tokio::sync::broadcast;

mod cache;
mod dialogs;
mod get;
#[cfg(any(debug_assertions, feature = "dev"))]
mod dev;
mod jam;
mod lyrics;
mod menu;
mod mascot;
mod profile;
mod thumbs;
mod update;

pub use get::downloads_notifier;

use meowtify_ui_view::*;

const LRU_CAP: usize = 256;
const ART_SIZE: u32 = 64;
const ROW_H: f32 = 56.0;
const PAGE: u32 = 50;
/// Load the next page when the viewport bottom is this many rows from the end.
const PREFETCH_ROWS: usize = 15;

thread_local! {
    /// The browser lives on the UI thread. Worker results find it here.
    static BROWSER: RefCell<Option<Rc<Browser>>> = const { RefCell::new(None) };
    /// What the queue panel shows now, so an unchanged queue does not reset its scroll position.
    static UP_NEXT: RefCell<Vec<(String, i32, Option<slint::Color>)>> = const { RefCell::new(Vec::new()) };
    /// The `Queue::rev` the panel was built from. The queue arrives every second; this tells an
    /// unchanged one apart without building anything.
    static UP_NEXT_REV: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// Autoplay's settings and ratings, for the buttons and the queue panel.
    static AUTOPLAY: RefCell<Option<meowtify_autoplay::Control>> = const { RefCell::new(None) };
    /// The model behind the queue panel, the video id of each of its songs, and the covers
    /// that arrived (so a rebuilt panel does not load them again).
    static UP_NEXT_MODEL: RefCell<Option<Rc<VecModel<UpNext>>>> = const { RefCell::new(None) };
    static UP_NEXT_VIDS: RefCell<Vec<Option<String>>> = const { RefCell::new(Vec::new()) };
    /// Cover image loader. Its answers find the pages that wait for them here.
    /// The animated logo and window icon. Play state changes reach it from `apply_event`.
    static MASCOT: RefCell<Option<Rc<mascot::Mascot>>> = const { RefCell::new(None) };
    pub(crate) static THUMBS: RefCell<Option<Rc<thumbs::Thumbs>>> = const { RefCell::new(None) };
    /// Set once the slider has taken the player's volume.
    static VOLUME_SYNCED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// What the UI shows and uses.
pub struct Services {
    /// Where the track list comes from.
    pub source: Arc<dyn MetadataSource>,
    /// The music folder. `None` hides the folder settings (mock mode).
    pub library: Option<Arc<Library>>,
    /// Provider search and downloads. `None` leaves the pages empty.
    pub downloads: Option<DownloadManager>,
    /// The yt-dlp program, so Settings can show its version and update it.
    pub yt_dlp: Option<Arc<meowtify_provider::YtDlp>>,
    /// Turns Discord presence on or off. `None` hides the setting.
    pub discord: Option<Arc<dyn Fn(bool) + Send + Sync>>,
    /// Turns autoplay on or off. `None` hides the setting.
    pub autoplay: Option<meowtify_autoplay::Control>,
    /// `meowtify://jam/…` links that were opened from outside the app. `None` ignores them.
    pub open_links: Option<tokio::sync::mpsc::UnboundedReceiver<String>>,
    /// The settings as read at startup. The window shows these and saves its own edits.
    pub settings: Settings,
}

/// Runs the window until it is closed. Must be called on the main thread.
pub fn run(
    handle: AppHandle,
    services: Services,
    mut events: broadcast::Receiver<Event>,
    rt: tokio::runtime::Handle,
) -> Result<(), slint::PlatformError> {
    let Services { source, library, downloads, yt_dlp, discord, autoplay, open_links, settings } = services;
    let win = MainWindow::new()?;
    win.set_touch(cfg!(target_os = "ios"));
    win.set_proxy(settings.proxy.as_str().into());
    win.set_app_version(
        format!("Meowtify v{}{}", env!("CARGO_PKG_VERSION"), if cfg!(debug_assertions) { " (debug build)" } else { "" }).into(),
    );
    {
        let weak = win.as_weak();
        win.on_set_proxy(move |text| {
            let Some(w) = weak.upgrade() else { return };
            match meowtify_provider::set_proxy(&text) {
                Ok(()) => {
                    let proxy = text.trim().to_string();
                    match Settings::update(|s| s.proxy = proxy.clone()) {
                        Ok(_) => {
                            w.set_proxy_bad(false);
                            w.set_proxy_note(if proxy.is_empty() { "No proxy.".into() } else { "Proxy applied.".into() });
                        }
                        Err(e) => {
                            w.set_proxy_bad(true);
                            w.set_proxy_note(format!("Applied, but could not save: {e}").into());
                        }
                    }
                }
                Err(e) => {
                    w.set_proxy_bad(true);
                    w.set_proxy_note(e.to_string().into());
                }
            }
        });
    }
    win.set_stream_playback(settings.stream_playback);
    {
        let weak = win.as_weak();
        win.on_set_stream_playback(move |on| update_settings(&weak, |s| s.stream_playback = on));
    }
    if let Some(control) = autoplay {
        AUTOPLAY.with(|a| *a.borrow_mut() = Some(control.clone()));
        win.set_autoplay_available(true);
        win.set_autoplay(settings.autoplay);
        win.set_autoplay_ahead(i32::from(settings.autoplay_ahead));
        win.set_autoplay_behind(i32::from(settings.autoplay_behind));
        win.set_lastfm_key(settings.lastfm_key.as_str().into());
        {
            let (weak, control) = (win.as_weak(), control.clone());
            win.on_set_autoplay(move |on| {
                control.set_enabled(on);
                update_settings(&weak, |s| s.autoplay = on);
            });
        }
        {
            let (weak, control) = (win.as_weak(), control.clone());
            win.on_set_autoplay_ahead(move |n| {
                let n = n.clamp(0, 255) as u8;
                control.set_ahead(n);
                update_settings(&weak, |s| s.autoplay_ahead = n);
            });
        }
        {
            let (weak, control) = (win.as_weak(), control.clone());
            win.on_set_autoplay_behind(move |n| {
                let n = n.clamp(0, 255) as u8;
                control.set_behind(n);
                update_settings(&weak, |s| s.autoplay_behind = n);
            });
        }
        {
            let weak = win.as_weak();
            win.on_set_lastfm_key(move |text| {
                let key = text.trim().to_string();
                let Some(w) = weak.upgrade() else { return };
                if !key.is_empty() && !meowtify_provider::valid_lastfm_key(&key) {
                    w.set_lastfm_bad(true);
                    w.set_lastfm_note("A Last.fm key is 32 letters and digits.".into());
                    return;
                }
                control.set_lastfm_key(&key);
                w.set_lastfm_bad(false);
                w.set_lastfm_note(if key.is_empty() { "Last.fm is off.".into() } else { "Last.fm is on.".into() });
                update_settings(&weak, |s| s.lastfm_key = key);
            });
        }
    }
    if let Some(library) = library.clone() {
        let rt_keep = rt.clone();
        // Copies the playing autoplay song from the cache into the music folder.
        let weak = win.as_weak();
        win.on_keep_now(move || {
            let Some(w) = weak.upgrade() else { return };
            let from = std::path::PathBuf::from(w.get_now_uri().as_str());
            let Some(dir) = library.dir() else {
                w.set_status("Choose a music folder in Settings first.".into());
                return;
            };
            let Some(name) = from.file_name() else { return };
            let to = dir.join(name);
            let uri = w.get_now_uri().to_string();
            let (library, weak) = (library.clone(), weak.clone());
            // The copy and the tag read can be slow (a big file, a slow disk): not on the UI thread.
            rt_keep.spawn_blocking(move || {
                let copied = if to.exists() { Ok(()) } else { std::fs::copy(&from, &to).map(|_| ()) };
                let added = copied.as_ref().ok().and_then(|_| library.add_file(&to));
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(w) = weak.upgrade() else { return };
                    match (copied, added) {
                        (Ok(()), Some(_)) => {
                            w.set_now_kept(true);
                            // Keeping a song tells autoplay that it was a good pick.
                            AUTOPLAY.with(|a| {
                                if let Some(c) = a.borrow().as_ref() {
                                    c.rate(&uri, true);
                                }
                            });
                            BROWSER.with(|b| {
                                if let Some(b) = b.borrow().as_ref() {
                                    b.library_changed();
                                }
                            });
                        }
                        (Ok(()), None) => w.set_status("could not keep the song: the copy is not readable as music".into()),
                        (Err(e), _) => w.set_status(format!("could not keep the song: {e}").into()),
                    }
                });
            });
        });
    }
    if let Some(set) = discord {
        win.set_discord_available(true);
        win.set_discord(settings.discord);
        let weak = win.as_weak();
        win.on_set_discord(move |on| {
            set(on);
            update_settings(&weak, |s| s.discord = on);
        });
    }
    let thumbs = Rc::new(thumbs::Thumbs::new(rt.clone()));
    THUMBS.with(|t| *t.borrow_mut() = Some(thumbs.clone()));
    cache::wire(&win, &rt, thumbs.disk());
    let model = TrackModel::new(thumbs.clone());
    {
        // Weak: the model owns the thumbnails, whose listener list must not own the model.
        let model = Rc::downgrade(&model);
        thumbs.listen(move |id, img| {
            if let Some(m) = model.upgrade() {
                m.thumb_ready(id, img);
            }
        });
    }
    thumbs.listen(|id, img| queue_art_ready(id, img));
    win.set_rows(ModelRc::from(model.clone()));
    let mascot = mascot::Mascot::new(&win);
    mascot.start();
    MASCOT.with(|m| *m.borrow_mut() = Some(mascot));
    {
        // The cover of the playing song arrives like any other thumbnail.
        let weak = win.as_weak();
        thumbs.listen(move |id, img| {
            if let (Some(w), Some(img)) = (weak.upgrade(), img) {
                if w.get_now_vid().as_str() == id {
                    w.set_now_art(img.clone());
                }
            }
        });
    }

    let browser = Rc::new(Browser {
        win: win.as_weak(),
        source,
        library,
        rt: rt.clone(),
        model,
        playlist_ids: RefCell::new(Vec::new()),
        pending: RefCell::new(None),
        pending_delete: RefCell::new(None),
        draft: RefCell::new(menu::Draft::default()),
        custom: RefCell::new(HashMap::new()),
        toast_timer: slint::Timer::default(),
        menu_target: RefCell::new(None),
        thumbs: thumbs.clone(),
        source_art: Rc::new(VecModel::default()),
        cover_rows: RefCell::new(HashMap::new()),
        cover_failed: RefCell::new(std::collections::HashSet::new()),
        st: RefCell::new(St { gen: 0, kind: Kind::Liked, loaded: 0, loading: false, done: false }),
    });
    BROWSER.with(|b| *b.borrow_mut() = Some(browser.clone()));
    win.set_source_art(ModelRc::from(browser.source_art.clone()));
    {
        let weak = Rc::downgrade(&browser);
        thumbs.listen(move |id, img| {
            if let Some(b) = weak.upgrade() {
                b.cover_ready(id, img);
            }
        });
    }

    let send = {
        let handle = handle.clone();
        let rt = rt.clone();
        move |cmd: Command| {
            // In a jam as a guest, the host decides what plays.
            let Some(cmd) = jam::filter(cmd) else { return };
            let handle = handle.clone();
            rt.spawn(async move {
                let _ = handle.send(cmd).await;
            });
        }
    };

    {
        let browser = browser.clone();
        update::install(&win, rt.clone(), Rc::new(move |text| browser.toast(text)));
    }
    {
        let browser = browser.clone();
        jam::install(&win, handle.clone(), browser.library.clone(), rt.clone(), Rc::new(move |text| browser.toast(text)), open_links, settings);
    }
    {
        // A song in the queue panel was clicked: play it now.
        let send = send.clone();
        let browser = browser.clone();
        win.on_jump_to(move |i| {
            let uri = usize::try_from(i).ok().and_then(|i| UP_NEXT.with(|c| c.borrow().get(i).map(|(u, _, _)| u.clone())));
            if let Some(uri) = uri {
                let title = UP_NEXT_MODEL.with(|m| {
                    m.borrow().as_ref().and_then(|m| m.row_data(i as usize)).map(|r| r.title.to_string()).unwrap_or_default()
                });
                browser.toast(&format!("Playing {title}"));
                send(Command::Jump(uri.into()));
            }
        });
    }
    {
        // Dislike: autoplay learns from it and the song is skipped. A toast says it was heard.
        let weak = win.as_weak();
        let browser = browser.clone();
        let send = send.clone();
        win.on_dislike_now(move || {
            let Some(w) = weak.upgrade() else { return };
            let uri = w.get_now_uri().to_string();
            AUTOPLAY.with(|a| {
                if let Some(c) = a.borrow().as_ref() {
                    c.rate(&uri, false);
                }
            });
            browser.toast("Disliked. Skipping, and fewer songs like this");
            send(Command::Next);
        });
    }

    if let Some(manager) = downloads {
        let get = get::Get::new(&win, manager, browser.library.clone(), thumbs.clone(), rt.clone(), Rc::new(send.clone()));
        get::GET.with(|g| *g.borrow_mut() = Some(get.clone()));
        {
            let get = get.clone();
            win.on_load_more(move || get.load_more());
        }
        {
            let get = get.clone();
            win.on_yt_search(move |t| get.search_now(&t));
        }
        {
            let get = get.clone();
            win.on_query_edited(move |t| get.query_edited(&t));
        }
        {
            let get = get.clone();
            win.on_suggest_request(move |t| get.suggest_request(&t));
        }
        {
            let get = get.clone();
            win.on_play_result(move |i| {
                if let Ok(i) = usize::try_from(i) {
                    get.play(i);
                }
            });
        }
        {
            let get = get.clone();
            win.on_result_hovered(move |i| {
                if let Ok(i) = usize::try_from(i) {
                    get.hovered(i);
                }
            });
        }
        {
            let get = get.clone();
            win.on_download_result(move |i| {
                if let Ok(i) = usize::try_from(i) {
                    get.download(i);
                }
            });
        }
        {
            let get = get.clone();
            win.on_cancel_job(move |i| {
                if let Ok(i) = usize::try_from(i) {
                    get.cancel(i);
                }
            });
        }
        {
            let get = get.clone();
            win.on_clear_finished(move || get.clear_finished());
        }
    }

    if let Some(yt) = yt_dlp {
        let show = {
            let weak = win.as_weak();
            move |text: String| {
                let _ = weak.upgrade_in_event_loop(move |w| {
                    w.set_tool_info(text.into());
                    w.set_tool_busy(false);
                });
            }
        };
        // Runs the update. Used by the button and once at startup, so yt-dlp stays
        // current without the user knowing it exists.
        let update = {
            let (yt, show, rt) = (yt.clone(), show.clone(), rt.clone());
            Rc::new(move |weak: slint::Weak<MainWindow>| {
                let Some(w) = weak.upgrade() else { return };
                if w.get_tool_busy() {
                    return;
                }
                w.set_tool_busy(true);
                w.set_tool_info("Updating yt-dlp\u{2026}".into());
                let (yt, show) = (yt.clone(), show.clone());
                rt.spawn(async move {
                    match yt.update().await {
                        Ok(o) if o.changed => show(format!("Updated to yt-dlp {}", o.version)),
                        Ok(o) => show(format!("yt-dlp {} (up to date)", o.version)),
                        Err(e) => {
                            let v = yt.version().await.map_or("not installed".to_string(), |v| format!("version {v}"));
                            show(format!("Update failed ({v}): {e}"))
                        }
                    }
                });
            })
        };
        {
            let update = update.clone();
            let weak = win.as_weak();
            win.on_update_tools(move || update(weak.clone()));
        }
        update(win.as_weak());
    }

    {
        let send = send.clone();
        let weak = win.as_weak();
        win.on_play_pause(move || {
            let playing = weak.upgrade().map(|w| w.get_playing()).unwrap_or(false);
            send(if playing { Command::Pause } else { Command::Play });
        });
    }
    {
        let send = send.clone();
        win.on_next(move || send(Command::Next));
    }
    {
        let send = send.clone();
        let weak = win.as_weak();
        win.on_seek(move |f| {
            let Some(w) = weak.upgrade() else { return };
            let total = w.get_total_ms();
            if total > 0 {
                let f = f.clamp(0.0, 1.0);
                w.set_progress(f);
                send(Command::Seek((f * total as f32) as u32));
            }
        });
    }
    {
        let send = send.clone();
        let weak = win.as_weak();
        win.on_toggle_shuffle(move || {
            if let Some(w) = weak.upgrade() {
                let on = !w.get_shuffle();
                w.set_shuffle(on);
                send(Command::SetShuffle(on));
            }
        });
    }
    {
        let send = send.clone();
        let weak = win.as_weak();
        win.on_cycle_repeat(move || {
            if let Some(w) = weak.upgrade() {
                // Off, then the whole queue, then one track.
                let (next, mode) = match w.get_repeat_mode() {
                    0 => (1, Repeat::Context),
                    1 => (2, Repeat::Track),
                    _ => (0, Repeat::Off),
                };
                w.set_repeat_mode(next);
                send(Command::SetRepeat(mode));
            }
        });
    }
    {
        let send = send.clone();
        win.on_set_volume(move |v| send(Command::SetVolume(v.clamp(0, 100) as u8)));
    }
    {
        let send = send.clone();
        win.on_previous(move || send(Command::Previous));
    }
    {
        let send = send.clone();
        let browser = browser.clone();
        win.on_row_clicked(move |i| {
            if let Some(t) = usize::try_from(i).ok().and_then(|i| browser.model.get(i)) {
                send(browser.play_command(&t.uri));
            }
        });
    }
    {
        let browser = browser.clone();
        win.on_source_selected(move |i| {
            let kind = match usize::try_from(i).ok() {
                Some(0) | None => Kind::Liked,
                Some(k) => match browser.playlist_ids.borrow().get(k - 1) {
                    Some(id) => Kind::Playlist(id.clone()),
                    None => return,
                },
            };
            browser.start(kind);
        });
    }
    {
        let browser = browser.clone();
        win.on_choose_folder(move || browser.choose_folder());
    }
    {
        let browser = browser.clone();
        let send: Rc<dyn Fn(Command)> = Rc::new(send.clone());
        win.on_ctx(move |kind, idx, action, arg| browser.context_action(&send, kind, idx, &action, arg));
    }
    {
        let browser = browser.clone();
        win.on_menu_opened(move |kind, idx| browser.capture_menu(kind, idx));
    }
    {
        let browser = browser.clone();
        win.on_picker_filter(move |text| browser.filter_playlists(&text));
    }
    {
        let browser = browser.clone();
        win.on_name_submitted(move |name| browser.name_submitted(&name));
    }
    {
        let browser = browser.clone();
        let send: Rc<dyn Fn(Command)> = Rc::new(send.clone());
        win.on_confirm_accepted(move || browser.delete_confirmed(&send));
    }
    {
        let browser = browser.clone();
        win.on_pick_cover(move || browser.pick_cover());
    }
    {
        let browser = browser.clone();
        win.on_clear_cover(move || browser.clear_cover_draft());
    }

    // Load more rows when the viewport nears the end of what is loaded.
    let scroll_timer = slint::Timer::default();
    {
        let browser = browser.clone();
        scroll_timer.start(slint::TimerMode::Repeated, Duration::from_millis(200), move || {
            browser.check_scroll();
        });
    }

    let weak = win.as_weak();
    rt.spawn(async move {
        loop {
            let event = match events.recv().await {
                Ok(e) => e,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            };
            let weak = weak.clone();
            if slint::invoke_from_event_loop(move || {
                jam::on_core_event(&event);
                if let Some(w) = weak.upgrade() {
                    apply_event(&w, event);
                }
            })
            .is_err()
            {
                return; // event loop is gone
            }
        }
    });

    lyrics::install(&win, rt.clone());

    // Screenshot and test aids; see `dev`. The timer lives as long as the window.
    #[cfg(any(debug_assertions, feature = "dev"))]
    let _dev_timer = dev::install(&win);

    browser.load_playlists();
    match &browser.library {
        Some(l) if l.dir().is_some() => browser.reload_library(),
        Some(_) => {
            win.set_onboarding_open(true);
            win.set_library_info("Choose the folder where your music is stored.".into());
        }
        None => browser.start(Kind::Liked), // mock source: no folder to manage
    }
    send(Command::Refresh);
    let result = win.run();
    BROWSER.with(|b| *b.borrow_mut() = None);
    THUMBS.with(|t| *t.borrow_mut() = None);
    MASCOT.with(|m| *m.borrow_mut() = None);
    get::GET.with(|g| *g.borrow_mut() = None);
    // The jam holds a handle to the player; dropping it lets the player's tasks end.
    jam::clear();
    result
}

#[derive(Clone)]
enum Kind {
    Liked,
    Playlist(Arc<str>),
}

struct St {
    /// Bumped on every source change so late results are dropped.
    gen: u64,
    kind: Kind,
    loaded: usize,
    loading: bool,
    done: bool,
}

struct Browser {
    /// What the name dialog is for while it is open.
    pending: RefCell<Option<menu::Pending>>,
    /// The song file waiting for the delete confirmation.
    pending_delete: RefCell<Option<Arc<str>>>,
    /// The cover picked in the name dialog, until it is confirmed.
    draft: RefCell<menu::Draft>,
    /// Chosen playlist cover files that were read, by path.
    custom: RefCell<HashMap<String, Image>>,
    toast_timer: slint::Timer,
    thumbs: Rc<thumbs::Thumbs>,
    /// Playlist covers, parallel to the sidebar list (entry 0 is All Songs).
    source_art: Rc<VecModel<Image>>,
    /// The item the open context menu belongs to, noted when it opened.
    menu_target: RefCell<Option<menu::MenuTarget>>,
    /// For each song cover a playlist collage uses, the sidebar rows (as in `source_art`) that
    /// show it, so an arriving cover updates only those. Failed covers are not asked for again.
    cover_rows: RefCell<HashMap<String, Vec<usize>>>,
    cover_failed: RefCell<std::collections::HashSet<String>>,
    win: slint::Weak<MainWindow>,
    source: Arc<dyn MetadataSource>,
    library: Option<Arc<Library>>,
    rt: tokio::runtime::Handle,
    model: Rc<TrackModel>,
    playlist_ids: RefCell<Vec<Arc<str>>>,
    st: RefCell<St>,
}

impl Browser {
    /// Asks for a music folder, saves it, and rescans.
    fn choose_folder(self: &Rc<Self>) {
        let Some(library) = &self.library else { return };
        let Some(path) = crate::dialogs::pick_folder("Choose your music folder", library.dir()) else { return };
        library.set_dir(Some(path.clone()));
        update_settings(&self.win, |s| s.library_dir = Some(path));
        if let Some(w) = self.win.upgrade() {
            w.set_onboarding_open(false);
        }
        self.reload_library();
    }

    /// Rescans the folder off the UI thread, then reloads the list.
    fn reload_library(self: &Rc<Self>) {
        let Some(library) = self.library.clone() else { return };
        if let Some(w) = self.win.upgrade() {
            w.set_library_path(library.dir().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default().into());
            w.set_library_info("Scanning…".into());
        }
        self.rt.spawn_blocking(move || {
            let n = library.rescan_blocking();
            let _ = slint::invoke_from_event_loop(move || {
                BROWSER.with(|b| {
                    if let Some(b) = b.borrow().as_ref() {
                        b.library_loaded(n);
                    }
                });
            });
        });
    }

    /// A file was added to the library without a rescan: refresh the list when it shows all songs.
    fn library_changed(self: &Rc<Self>) {
        if let Some(lib) = &self.library {
            if let Some(w) = self.win.upgrade() {
                w.set_library_info(format!("{} tracks", lib.tracks().len()).into());
            }
        }
        let kind = self.st.borrow().kind.clone();
        self.start(kind);
    }

    fn library_loaded(self: &Rc<Self>, count: usize) {
        if let Some(w) = self.win.upgrade() {
            let msg = match count {
                0 => "No music found in this folder yet.".to_string(),
                1 => "1 track".to_string(),
                n => format!("{n} tracks"),
            };
            w.set_library_info(msg.into());
        }
        self.start(Kind::Liked);
        get::GET.with(|g| {
            if let Some(g) = g.borrow().as_ref() {
                g.library_ready();
            }
        });
    }

    fn start(self: &Rc<Self>, kind: Kind) {
        {
            let mut s = self.st.borrow_mut();
            s.gen += 1;
            s.kind = kind.clone();
            s.loaded = 0;
            s.loading = false;
            s.done = false;
        }
        self.model.reset();
        if let Some(w) = self.win.upgrade() {
            w.set_viewing_playlist(matches!(kind, Kind::Playlist(_)));
            w.set_scroll_y(0.0);
            w.set_status(SharedString::default());
        }
        self.load_more();
    }

    fn load_more(self: &Rc<Self>) {
        let (kind, offset, gen) = {
            let mut s = self.st.borrow_mut();
            if s.loading || s.done {
                return;
            }
            s.loading = true;
            (s.kind.clone(), s.loaded as u32, s.gen)
        };
        if let Some(w) = self.win.upgrade() {
            w.set_loading(true);
        }
        let source = self.source.clone();
        self.rt.spawn(async move {
            let result = match &kind {
                Kind::Liked => source.saved_tracks(offset, PAGE).await,
                Kind::Playlist(id) => source.playlist_items(id, offset, PAGE).await,
            };
            let _ = slint::invoke_from_event_loop(move || {
                BROWSER.with(|b| {
                    if let Some(b) = b.borrow().as_ref() {
                        b.apply_page(gen, result);
                    }
                });
            });
        });
    }

    fn apply_page(&self, gen: u64, result: meowtify_core::Result<meowtify_core::Page<Track>>) {
        let mut s = self.st.borrow_mut();
        if gen != s.gen {
            return;
        }
        s.loading = false;
        let mut status = None;
        match result {
            Ok(page) => {
                s.loaded += page.items.len();
                s.done = !page.has_next || page.items.is_empty();
                self.model.append(page.items);
            }
            Err(e) => {
                s.done = true; // do not retry in a loop; picking a source retries
                status = Some(e.to_string());
            }
        }
        drop(s);
        if let Some(w) = self.win.upgrade() {
            w.set_loading(false);
            if let Some(m) = status {
                w.set_status(m.into());
            }
        }
    }

    fn check_scroll(self: &Rc<Self>) {
        let Some(w) = self.win.upgrade() else { return };
        let view_h = w.window().size().height as f32 / w.window().scale_factor();
        let bottom_row = ((-w.get_scroll_y() + view_h) / ROW_H).max(0.0) as usize;
        if bottom_row + PREFETCH_ROWS >= self.st.borrow().loaded {
            self.load_more();
        }
    }

    fn load_playlists(&self) {
        let source = self.source.clone();
        self.rt.spawn(async move {
            // All pages: a sidebar with only the first page would hide later playlists.
            let mut items: Vec<Playlist> = Vec::new();
            let mut ok = true;
            loop {
                match source.user_playlists(items.len() as u32, PAGE).await {
                    Ok(page) => {
                        let more = page.has_next && !page.items.is_empty();
                        items.extend(page.items);
                        if !more {
                            break;
                        }
                    }
                    Err(_) => {
                        ok = false;
                        break;
                    }
                }
            }
            let _ = slint::invoke_from_event_loop(move || {
                BROWSER.with(|b| {
                    if let (Some(b), true) = (b.borrow().as_ref(), ok) {
                        b.apply_playlists(items);
                    }
                });
            });
        });
    }

    fn apply_playlists(&self, items: Vec<Playlist>) {
        let mut names: Vec<SharedString> = vec!["All Songs".into()];
        let mut ids = Vec::with_capacity(items.len());
        for p in items {
            names.push(p.name.as_ref().into());
            ids.push(p.id);
        }
        let open = match &self.st.borrow().kind {
            Kind::Playlist(id) => Some(ids.iter().position(|p| p == id)),
            Kind::Liked => None,
        };
        *self.playlist_ids.borrow_mut() = ids;
        if let Some(w) = self.win.upgrade() {
            w.set_sources(ModelRc::new(VecModel::from(names)));
            // Keep the highlighted entry on the open playlist when the list shifts.
            if let Some(Some(pos)) = open {
                w.set_current_source(pos as i32 + 1);
            }
        }
        self.refresh_covers();
    }
}

/// Changes one value in the settings file. A failed save shows in the status line.
fn update_settings(weak: &slint::Weak<MainWindow>, change: impl FnOnce(&mut Settings)) {
    if let Err(e) = Settings::update(change) {
        if let Some(w) = weak.upgrade() {
            w.set_status(format!("could not save settings: {e}").into());
        }
    }
}

/// Songs shown in the queue panel at most.
const UP_NEXT_SHOWN: usize = 20;

fn grey_art() -> Image {
    let mut ph = SharedPixelBuffer::<Rgba8Pixel>::new(8, 8);
    ph.make_mut_slice().fill(Rgba8Pixel { r: 70, g: 70, b: 74, a: 255 });
    Image::from_rgba8(ph)
}

/// The cover of a YouTube video: the saved one, or grey while it loads.
fn cover_of(video_id: &str) -> Image {
    match THUMBS.with(|th| th.borrow().as_ref().and_then(|t| t.cached(video_id))) {
        Some(img) => img,
        None => {
            THUMBS.with(|th| {
                if let Some(th) = th.borrow().as_ref() {
                    th.fetch(video_id, &meowtify_core::youtube_thumbnail(video_id));
                }
            });
            grey_art()
        }
    }
}

/// A video id a jam host sent is used only when it has the form of a YouTube id.
fn valid_video_id(id: &str) -> bool {
    id.len() == 11 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A song of the host's queue as a jam guest shows it.
#[derive(Clone, PartialEq)]
pub(crate) struct JamQueueRow {
    pub title: String,
    pub artist: String,
    /// The video id of the cover, if the host sent one.
    pub key: Option<String>,
    /// The cat colour of whoever queued the song.
    pub mark: Option<slint::Color>,
}

/// Fills the queue panel of a jam guest with the host's queue.
pub(crate) fn show_jam_queue(w: &MainWindow, songs: &[JamQueueRow]) {
    let mut vids: Vec<Option<String>> = Vec::with_capacity(songs.len());
    let mut rows: Vec<UpNext> = Vec::with_capacity(songs.len());
    for JamQueueRow { title, artist, key, mark } in songs.iter().take(UP_NEXT_SHOWN) {
        let vid = key.clone().filter(|k| valid_video_id(k));
        let art = match &vid {
            Some(v) => cover_of(v),
            None => Image::from_rgba8(make_art(art_key(&format!("{title}\n{artist}")))),
        };
        vids.push(vid);
        rows.push(UpNext {
            title: title.as_str().into(),
            artist: artist.as_str().into(),
            kind: if mark.is_some() { 0 } else { 1 },
            art,
            has_mark: mark.is_some(),
            mark: mark.unwrap_or_default(),
        });
    }
    let model = Rc::new(VecModel::from(rows));
    UP_NEXT_MODEL.with(|m| *m.borrow_mut() = Some(model.clone()));
    UP_NEXT_VIDS.with(|v| *v.borrow_mut() = vids);
    w.set_up_next(ModelRc::from(model));
    // The next real queue event must draw again.
    UP_NEXT.with(|c| c.borrow_mut().clear());
    UP_NEXT_REV.set(0);
}

/// Fills the queue panel: the songs the user queued, then autoplay's, then the rest.
fn set_up_next(w: &MainWindow, q: &meowtify_core::Queue) {
    w.set_autoplay_busy(AUTOPLAY.with(|a| a.borrow().as_ref().is_some_and(|c| c.busy())));
    if jam::is_guest() {
        return; // the queue panel shows the host's queue
    }
    if q.rev != 0 && UP_NEXT_REV.get() == q.rev {
        return;
    }
    let shown: Vec<(String, i32, &Track)> = q
        .upcoming
        .iter()
        .take(UP_NEXT_SHOWN)
        .enumerate()
        .map(|(i, t)| (t.uri.to_string(), if i < q.manual { 0 } else if i < q.manual + q.auto { 1 } else { 2 }, t))
        .collect();
    // In a hosted jam the bar of a queued song has the colour of the cat that queued it.
    let marks: Vec<Option<slint::Color>> = shown.iter().map(|(u, k, _)| jam::queue_mark(u, *k == 0)).collect();
    let key: Vec<(String, i32, Option<slint::Color>)> = shown.iter().zip(&marks).map(|((u, k, _), m)| (u.clone(), *k, *m)).collect();
    if UP_NEXT.with(|c| *c.borrow() == key) {
        UP_NEXT_REV.set(q.rev);
        return;
    }
    let mut vids: Vec<Option<String>> = Vec::with_capacity(shown.len());
    let mut rows: Vec<UpNext> = Vec::with_capacity(shown.len());
    for ((_, kind, t), mark) in shown.iter().zip(&marks) {
        let vid = meowtify_core::youtube_id(&t.uri).map(str::to_string);
        let art = match &vid {
            Some(v) => cover_of(v),
            None => Image::from_rgba8(make_art(art_id(t))),
        };
        vids.push(vid);
        rows.push(UpNext { title: t.name.as_ref().into(), artist: artist_line(t).into(), kind: *kind, art, has_mark: mark.is_some(), mark: mark.unwrap_or_default() });
    }
    let model = Rc::new(VecModel::from(rows));
    UP_NEXT_MODEL.with(|m| *m.borrow_mut() = Some(model.clone()));
    UP_NEXT_VIDS.with(|v| *v.borrow_mut() = vids);
    w.set_up_next(ModelRc::from(model));
    UP_NEXT.with(|c| *c.borrow_mut() = key);
    UP_NEXT_REV.set(q.rev);
}

/// A cover arrived: put it on every queue entry that shows that video.
fn queue_art_ready(video_id: &str, img: Option<&Image>) {
    let Some(img) = img else { return };
    // Every cover of the app arrives here; only the ones a queue card shows matter.
    let rows: Vec<usize> =
        UP_NEXT_VIDS.with(|v| v.borrow().iter().enumerate().filter(|(_, x)| x.as_deref() == Some(video_id)).map(|(i, _)| i).collect());
    if rows.is_empty() {
        return;
    }
    let Some(model) = UP_NEXT_MODEL.with(|m| m.borrow().clone()) else { return };
    for i in rows {
        if let Some(mut row) = model.row_data(i) {
            row.art = img.clone();
            model.set_row_data(i, row);
        }
    }
}

fn apply_event(w: &MainWindow, event: Event) {
    match event {
        Event::PlaybackChanged(Some(state)) => {
            w.set_playing(state.is_playing);
            set_mascot_playing(state.is_playing);
            w.set_shuffle(state.shuffle);
            w.set_repeat_mode(match state.repeat {
                Repeat::Off => 0,
                Repeat::Context => 1,
                Repeat::Track => 2,
            });
            match &state.track {
                Some(t) => {
                    if w.get_now_uri().as_str() != t.uri.as_ref() {
                        w.set_now_uri(t.uri.as_ref().into());
                        // A song in the autoplay cache can be kept.
                        w.set_now_temp(std::path::Path::new(t.uri.as_ref()).starts_with(meowtify_autoplay::cache_dir()));
                        w.set_now_kept(false);
                        set_now_art(w, t);
                    }
                    w.set_total_ms(t.duration_ms.min(i32::MAX as u32) as i32);
                    w.set_elapsed(format_duration(state.progress_ms).into());
                    w.set_total(format_duration(t.duration_ms).into());
                    // Take the volume from the player once. After that the slider owns it.
                    VOLUME_SYNCED.with(|c| {
                        if !c.replace(true) {
                            if let Some(v) = state.volume {
                                w.set_volume(i32::from(v));
                            }
                        }
                    });
                    w.set_now_title(t.name.as_ref().into());
                    w.set_now_artist(artist_line(t).into());
                    let p = if t.duration_ms > 0 {
                        (state.progress_ms as f32 / t.duration_ms as f32).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    w.set_progress(p);
                }
                None => w.set_now_title(SharedString::default()),
            }
            w.set_status(SharedString::default());
            lyrics::on_playback(w, Some(&state));
        }
        Event::PlaybackChanged(None) => {
            w.set_playing(false);
            set_mascot_playing(false);
            w.set_now_uri(SharedString::default());
            w.set_now_temp(false);
            w.set_now_vid(SharedString::default());
            w.set_elapsed("0:00".into());
            w.set_total("0:00".into());
            w.set_total_ms(0);
            w.set_now_title("Nothing playing".into());
            w.set_now_artist(SharedString::default());
            w.set_progress(0.0);
            lyrics::on_playback(w, None);
        }
        Event::Error(e) => w.set_status(e.to_string().into()),
        Event::QueueChanged(q) => set_up_next(w, &q),
    }
}

/// Shows the cover of the song that just started: its thumbnail when the file
/// has a video id (the disk cache makes this instant), a gradient otherwise.
fn set_now_art(w: &MainWindow, t: &Track) {
    // A jam guest plays a cache file with no id in its name; the host sent the id.
    match meowtify_core::cover_id(&t.uri) {
        Some(v) => {
            w.set_now_vid(v.as_str().into());
            w.set_now_art(cover_of(&v));
        }
        None => {
            w.set_now_vid(SharedString::default());
            w.set_now_art(Image::from_rgba8(make_art(art_id(t))));
        }
    }
}

fn set_mascot_playing(on: bool) {
    MASCOT.with(|m| {
        if let Some(m) = m.borrow().as_ref() {
            m.set_playing(on);
        }
    });
}

fn artist_line(t: &Track) -> String {
    let mut s = String::new();
    for (i, a) in t.artists.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push_str(&a.name);
    }
    s
}

fn format_duration(ms: u32) -> String {
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}

/// FNV-1a. Stable art key from a string id.
fn art_key(id: &str) -> u32 {
    let mut h: u32 = 0x811C_9DC5;
    for b in id.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

fn art_id(t: &Track) -> u32 {
    match &t.album {
        Some(a) => art_key(&a.id),
        None => art_key(&t.id),
    }
}

fn make_art(key: u32) -> SharedPixelBuffer<Rgba8Pixel> {
    let mut s = key.wrapping_mul(0x9E37_79B9).wrapping_add(0x85EB_CA6B) | 1;
    let mut next = || {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        s
    };
    let c0 = [(next() & 255) as f32, (next() & 255) as f32, (next() & 255) as f32];
    let c1 = [(next() & 255) as f32, (next() & 255) as f32, (next() & 255) as f32];
    let ang = (next() % 628) as f32 / 100.0;
    let (dx, dy) = (ang.cos(), ang.sin());
    let half = ART_SIZE as f32 / 2.0;
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(ART_SIZE, ART_SIZE);
    let px = buf.make_mut_slice();
    for y in 0..ART_SIZE as usize {
        for x in 0..ART_SIZE as usize {
            let t = (((x as f32 - half) * dx + (y as f32 - half) * dy) / 90.0 + 0.5).clamp(0.0, 1.0);
            let l = |a: f32, b: f32| (a + (b - a) * t) as u8;
            px[y * ART_SIZE as usize + x] =
                Rgba8Pixel { r: l(c0[0], c1[0]), g: l(c0[1], c1[1]), b: l(c0[2], c1[2]), a: 255 };
        }
    }
    buf
}

struct Lru {
    map: HashMap<u32, (Image, u64)>,
    tick: u64,
}

impl Lru {
    fn get(&mut self, id: u32) -> Option<Image> {
        self.tick += 1;
        let t = self.tick;
        self.map.get_mut(&id).map(|e| {
            e.1 = t;
            e.0.clone()
        })
    }

    fn put(&mut self, id: u32, img: Image) {
        self.tick += 1;
        self.map.insert(id, (img, self.tick));
        while self.map.len() > LRU_CAP {
            let oldest = *self.map.iter().min_by_key(|(_, v)| v.1).unwrap().0;
            self.map.remove(&oldest);
        }
    }
}

/// Virtual list model. Rows are built on demand, art loads on worker threads.
struct TrackModel {
    notify: ModelNotify,
    tracks: RefCell<Vec<Track>>,
    lru: RefCell<Lru>,
    waiting: RefCell<HashMap<u32, Vec<usize>>>,
    placeholder: Image,
    thumbs: Rc<thumbs::Thumbs>,
}

impl TrackModel {
    fn new(thumbs: Rc<thumbs::Thumbs>) -> Rc<Self> {
        let mut ph = SharedPixelBuffer::<Rgba8Pixel>::new(ART_SIZE, ART_SIZE);
        ph.make_mut_slice().fill(Rgba8Pixel { r: 70, g: 70, b: 74, a: 255 });
        Rc::new(Self {
            notify: ModelNotify::default(),
            tracks: RefCell::new(Vec::new()),
            lru: RefCell::new(Lru { map: HashMap::new(), tick: 0 }),
            waiting: RefCell::new(HashMap::new()),
            placeholder: Image::from_rgba8(ph),
            thumbs,
        })
    }

    fn get(&self, i: usize) -> Option<Track> {
        self.tracks.borrow().get(i).cloned()
    }

    fn reset(&self) {
        self.tracks.borrow_mut().clear();
        self.waiting.borrow_mut().clear();
        self.notify.reset();
    }

    fn append(&self, items: Vec<Track>) {
        if items.is_empty() {
            return;
        }
        let (start, count) = {
            let mut t = self.tracks.borrow_mut();
            let start = t.len();
            let count = items.len();
            t.extend(items);
            (start, count)
        };
        self.notify.row_added(start, count);
    }

    /// A cover image arrived (or failed: `None` gives the generated gradient, kept here because
    /// the cover service has no image for it). Real covers stay in the cover service.
    fn thumb_ready(&self, video_id: &str, img: Option<&Image>) {
        let key = art_key(video_id);
        if img.is_none() {
            self.lru.borrow_mut().put(key, Image::from_rgba8(make_art(key)));
        }
        let rows = self.waiting.borrow_mut().remove(&key).unwrap_or_default();
        let len = self.tracks.borrow().len();
        for r in rows {
            if r < len {
                self.notify.row_changed(r);
            }
        }
    }
}

impl Model for TrackModel {
    type Data = Row;

    fn row_count(&self) -> usize {
        self.tracks.borrow().len()
    }

    fn row_data(&self, i: usize) -> Option<Row> {
        let t = self.get(i)?;
        let vid = meowtify_core::youtube_id(&t.uri);
        let key = vid.map(art_key).unwrap_or_else(|| art_id(&t));
        let cached = vid.and_then(|v| self.thumbs.cached(v)).or_else(|| self.lru.borrow_mut().get(key));
        let art = match (cached, vid) {
            (Some(img), _) => img,
            (None, Some(v)) => {
                let mut w = self.waiting.borrow_mut();
                let e = w.entry(key).or_default();
                if e.is_empty() {
                    self.thumbs.fetch(v, &meowtify_core::youtube_thumbnail(v));
                }
                if !e.contains(&i) {
                    e.push(i);
                }
                self.placeholder.clone()
            }
            (None, None) => {
                let img = Image::from_rgba8(make_art(key));
                self.lru.borrow_mut().put(key, img.clone());
                img
            }
        };
        Some(Row {
            key: t.uri.as_ref().into(),
            title: t.name.as_ref().into(),
            artist: artist_line(&t).into(),
            album: t.album.as_ref().map(|a| a.name.as_ref()).unwrap_or("").into(),
            duration: format_duration(t.duration_ms).into(),
            art,
        })
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
