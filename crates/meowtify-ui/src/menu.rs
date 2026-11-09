//! Right-click menus, playlists and the "added" feedback.

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use meowtify_core::{Command, Place, Selection};

use crate::{get, Browser, Kind};

/// The cover choice made in the dialog, applied when it is confirmed.
#[derive(Default)]
pub(crate) enum Draft {
    #[default]
    Unchanged,
    /// Back to the automatic cover.
    Clear,
    /// A new image: the processed PNG file contents.
    Set(Vec<u8>),
}

/// What the name dialog is for.
#[derive(Clone)]
pub(crate) enum Pending {
    /// A new playlist, optionally holding one track right away.
    New { track: Option<Arc<str>> },
    Rename { id: Arc<str> },
}

const TOAST_FOR: Duration = Duration::from_millis(1800);

/// What an open context menu acts on.
#[derive(Clone)]
pub(crate) enum MenuTarget {
    /// A library song, by file path.
    Track(Arc<str>),
    /// A search result, by provider id.
    Result(String),
    Playlist(Arc<str>),
}

/// The command that plays a song the user picked outside a playlist. With autoplay on the song
/// is the whole queue, so similar songs can follow it when it ends. Otherwise the library
/// plays on after it.
pub(crate) fn pick(autoplay: bool, uri: &Arc<str>) -> Command {
    Command::PlayFrom(if autoplay { Selection::Single(uri.clone()) } else { Selection::Library(uri.clone()) })
}

impl Browser {
    pub(crate) fn toast(&self, text: &str) {
        let Some(w) = self.win.upgrade() else { return };
        w.set_toast(text.into());
        w.set_toast_on(true);
        let weak = self.win.clone();
        self.toast_timer.start(slint::TimerMode::SingleShot, TOAST_FOR, move || {
            if let Some(w) = weak.upgrade() {
                w.set_toast_on(false);
            }
        });
    }

    /// Notes which item the menu that just opened belongs to. The row number alone is not
    /// enough: the list can change (a rescan, a new page, a search) while the menu is open.
    pub(crate) fn capture_menu(&self, kind: i32, idx: i32) {
        let Ok(idx) = usize::try_from(idx) else { return };
        let target = match kind {
            0 => self.model.get(idx).map(|t| MenuTarget::Track(t.uri.clone())),
            1 => get::GET.with(|g| g.borrow().as_ref().and_then(|g| g.item_id(idx))).map(MenuTarget::Result),
            2 => self.playlist_id_at(idx).map(MenuTarget::Playlist),
            3 => crate::UP_NEXT.with(|c| c.borrow().get(idx).map(|(u, _, _)| MenuTarget::Track(Arc::from(u.as_str())))),
            _ => None,
        };
        *self.menu_target.borrow_mut() = target;
    }

    /// The rows of the context menu for the item that was just long-pressed, for a platform that
    /// draws its own menu (iOS). The same labels, actions and red rows as the popup in app.slint.
    pub(crate) fn menu_rows(&self, kind: i32, idx: i32) -> Vec<crate::NativeRow> {
        use crate::NativeRow;
        let mut rows: Vec<NativeRow> = Vec::new();
        let at = usize::try_from(idx).ok();
        let installed = match kind {
            0 | 3 => true,
            1 => match at.and_then(|i| self.menu_track_uri(1, i)) {
                Some(uri) => self.library.as_ref().is_some_and(|l| l.find(&uri).is_some()),
                None => false,
            },
            _ => false,
        };
        let row = |label: &str, action: &'static str, arg: i32, danger: bool| NativeRow { label: label.to_string(), action, arg, danger };
        if kind == 2 {
            rows.push(row("Edit\u{2026}", "rename", 0, false));
            rows.push(row("Export\u{2026}", "export", 0, false));
            rows.push(row("Delete playlist", "delete", 0, true));
            return rows;
        }
        rows.push(row("Play", "play", 0, false));
        if kind == 1 && !installed {
            rows.push(row("Download", "download", 0, false));
        }
        if installed {
            rows.push(row("Play next", "next", 0, false));
            rows.push(row("Add to queue", "queue", 0, false));
            // Every playlist is in the menu, as in the popup's scrolling list.
            if let Some(library) = &self.library {
                for (i, id) in self.playlist_ids.borrow().iter().enumerate() {
                    if let Some(p) = library.playlists().get(id) {
                        rows.push(row(&p.name, "add", i as i32, false));
                    }
                }
            }
            rows.push(row("New playlist\u{2026}", "new", -1, false));
        }
        if kind < 2 && installed {
            rows.push(row("Delete song\u{2026}", "delete", 0, true));
        }
        if kind == 0 && matches!(self.st.borrow().kind, Kind::Playlist(_)) {
            rows.push(row("Remove from playlist", "remove", 0, true));
        }
        rows
    }

    /// A menu was opened on iOS: the platform draws it, and the chosen row goes through
    /// `context_action`, the same way the popup's `ctx` does. Nothing happens without the hook.
    pub(crate) fn native_menu(self: &Rc<Self>, send: &Rc<dyn Fn(Command)>, kind: i32, idx: i32) {
        let Some(show) = crate::NATIVE_MENU.with(|m| m.borrow().clone()) else { return };
        let rows = self.menu_rows(kind, idx);
        if rows.is_empty() {
            return;
        }
        let browser = Rc::downgrade(self);
        let send = send.clone();
        let present = move || {
            let Some(b) = browser.upgrade() else { return };
            let browser = Rc::downgrade(&b);
            let send = send.clone();
            show(rows.clone(), Box::new(move |chosen| {
                let Some(row) = rows.get(chosen) else { return };
                let (action, arg) = (row.action, row.arg);
                if let Some(b) = browser.upgrade() {
                    b.context_action(&send, kind, idx, action, arg);
                }
            }));
        };
        // `menu-opened` also comes from the Export button, which notes the target and then acts in
        // the same turn. Showing the menu only after the turn lets that `ctx` cancel it.
        crate::PENDING_MENU.with(|p| *p.borrow_mut() = Some(Box::new(present)));
        crate::MENU_TIMER.with(|t| {
            t.start(slint::TimerMode::SingleShot, Duration::from_millis(0), || {
                let pending = crate::PENDING_MENU.with(|p| p.borrow_mut().take());
                if let Some(pending) = pending {
                    pending();
                }
            });
        });
    }

    /// The playlists whose name contains `text` (any case), for the "add to playlist" search.
    pub(crate) fn filter_playlists(&self, text: &str) {
        let Some(library) = &self.library else { return };
        let needle = text.trim().to_lowercase();
        let rows: Vec<crate::PickRow> = self
            .playlist_ids
            .borrow()
            .iter()
            .enumerate()
            .filter_map(|(i, id)| {
                let name = library.playlists().get(id)?.name;
                (needle.is_empty() || name.to_lowercase().contains(&needle)).then(|| crate::PickRow { name: name.as_str().into(), index: i as i32 })
            })
            .collect();
        if let Some(w) = self.win.upgrade() {
            w.set_picker_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
        }
    }

    /// File path of the song a menu row stands for, when it is in the library.
    fn menu_track_uri(&self, kind: i32, idx: usize) -> Option<Arc<str>> {
        match (kind, self.menu_target.borrow().as_ref()) {
            (0 | 3, Some(MenuTarget::Track(uri))) => Some(uri.clone()),
            (1, Some(MenuTarget::Result(id))) => get::GET.with(|g| g.borrow().as_ref().and_then(|g| g.uri_of_id(id))),
            (_, Some(_)) => None,
            (0, None) => self.model.get(idx).map(|t| t.uri.clone()),
            (1, None) => get::GET.with(|g| g.borrow().as_ref().and_then(|g| g.result_uri(idx))),
            _ => None,
        }
    }

    /// The playlist a sidebar menu stands for.
    fn menu_playlist(&self, idx: Option<usize>) -> Option<Arc<str>> {
        match self.menu_target.borrow().as_ref() {
            Some(MenuTarget::Playlist(id)) => Some(id.clone()),
            _ => idx.and_then(|i| self.playlist_id_at(i)),
        }
    }

    /// Current row of the search result the menu stands for.
    fn menu_result_row(&self, idx: usize) -> Option<usize> {
        match self.menu_target.borrow().as_ref() {
            Some(MenuTarget::Result(id)) => get::GET.with(|g| g.borrow().as_ref().and_then(|g| g.item_index(id))),
            _ => Some(idx),
        }
    }

    fn playlist_id_at(&self, i: usize) -> Option<Arc<str>> {
        self.playlist_ids.borrow().get(i).cloned()
    }

    fn current_playlist(&self) -> Option<Arc<str>> {
        match &self.st.borrow().kind {
            Kind::Playlist(id) => Some(id.clone()),
            Kind::Liked => None,
        }
    }

    /// Reloads the sidebar list, and the open playlist when it changed.
    fn playlists_changed(self: &Rc<Self>, refresh_open: bool) {
        // A change that could not be written stays in memory only; say so.
        if let Some(e) = self.library.as_ref().and_then(|l| l.playlists().save_error()) {
            self.toast(&e);
        }
        self.load_playlists();
        if refresh_open {
            if let Some(id) = self.current_playlist() {
                self.start(Kind::Playlist(id));
            }
        }
    }

    pub(crate) fn context_action(self: &Rc<Self>, send: &Rc<dyn Fn(Command)>, kind: i32, idx: i32, action: &str, arg: i32) {
        // A menu that was about to be shown is not wanted: this is the action it stood for.
        crate::PENDING_MENU.with(|p| *p.borrow_mut() = None);
        let idx = usize::try_from(idx).ok();
        let Some(library) = self.library.clone() else { return };
        match (kind, action) {
            (1, "download") => {
                if let Some(i) = idx.and_then(|i| self.menu_result_row(i)) {
                    get::GET.with(|g| {
                        if let Some(g) = g.borrow().as_ref() {
                            g.download(i);
                        }
                    });
                }
            }
            (1, "play") => {
                if let Some(i) = idx.and_then(|i| self.menu_result_row(i)) {
                    get::GET.with(|g| {
                        if let Some(g) = g.borrow().as_ref() {
                            g.play(i);
                        }
                    });
                }
            }
            (0, "play") => {
                let Some(uri) = idx.and_then(|i| self.menu_track_uri(0, i)) else { return };
                send(self.play_command(&uri));
            }
            (3, "play") => {
                let Some(uri) = idx.and_then(|i| self.menu_track_uri(3, i)) else { return };
                send(Command::Jump(uri));
            }
            (0 | 1 | 3, "next") | (0 | 1 | 3, "queue") => {
                let Some(uri) = idx.and_then(|i| self.menu_track_uri(kind, i)) else { return };
                if action == "next" {
                    send(Command::Enqueue { uri, place: Place::Next });
                    self.toast("Playing next");
                } else {
                    send(Command::Enqueue { uri, place: Place::Queue });
                    self.toast("Added to queue");
                }
            }
            (0 | 1 | 3, "add") => {
                let Some(uri) = idx.and_then(|i| self.menu_track_uri(kind, i)) else { return };
                let Some(id) = usize::try_from(arg).ok().and_then(|a| self.playlist_id_at(a)) else { return };
                let name = library.playlists().get(&id).map(|p| p.name).unwrap_or_default();
                if library.playlists().add(&id, &uri) {
                    self.toast(&format!("Added to {name}"));
                } else {
                    self.toast(&format!("Already in {name}"));
                }
                self.playlists_changed(self.current_playlist().as_deref() == Some(&*id));
            }
            (0 | 1, "delete") => {
                let Some(uri) = idx.and_then(|i| self.menu_track_uri(kind, i)) else { return };
                let name = library.find(&uri).map(|t| t.name.to_string()).unwrap_or_default();
                *self.pending_delete.borrow_mut() = Some(uri);
                if let Some(w) = self.win.upgrade() {
                    w.set_confirm_title(format!("Delete \u{201c}{name}\u{201d}?").into());
                    w.set_confirm_text("The file is deleted from your music folder and removed from your playlists. This cannot be undone.".into());
                    w.set_confirm_open(true);
                }
            }
            (0 | 1 | 3, "new") => {
                let track = idx.and_then(|i| self.menu_track_uri(kind, i));
                self.ask_name(Pending::New { track }, "New playlist", "", None);
            }
            (2, "new") => self.ask_name(Pending::New { track: None }, "New playlist", "", None),
            (2, "rename") => {
                let Some(id) = self.menu_playlist(idx) else { return };
                let p = library.playlists().get(&id);
                let name = p.as_ref().map(|p| p.name.clone()).unwrap_or_default();
                let cover = p.and_then(|p| p.cover);
                self.ask_name(Pending::Rename { id }, "Edit playlist", &name, cover.as_deref());
            }
            (2, "export") => {
                let Some(id) = self.menu_playlist(idx) else { return };
                self.export_playlist(&library, &id);
            }
            (2, "delete") => {
                let Some(id) = self.menu_playlist(idx) else { return };
                let was_open = self.current_playlist().as_deref() == Some(&*id);
                if let Some(cover) = library.playlists().delete(&id) {
                    let _ = std::fs::remove_file(cover);
                }
                if was_open {
                    if let Some(w) = self.win.upgrade() {
                        w.set_current_source(0);
                    }
                    self.start(Kind::Liked);
                }
                self.load_playlists();
                self.toast("Playlist deleted");
            }
            (0, "remove") => {
                let (Some(uri), Some(id)) = (idx.and_then(|i| self.menu_track_uri(0, i)), self.current_playlist()) else { return };
                library.playlists().remove(&id, &uri);
                self.playlists_changed(true);
            }
            _ => {}
        }
    }

    /// Asks where to save, then writes the playlist as an .m3u8 file off the UI thread.
    fn export_playlist(self: &Rc<Self>, library: &Arc<meowtify_library::Library>, id: &str) {
        let Some(p) = library.playlists().get(id) else { return };
        if p.tracks.is_empty() {
            self.toast("This playlist is empty");
            return;
        }
        let safe: String = p.name.chars().map(|c| if "<>:\"/\\|?*".contains(c) || c.is_control() { '_' } else { c }).collect();
        let name = format!("{}.m3u8", safe.trim());
        let Some(path) = crate::dialogs::save_file("Export playlist", &name, ("Playlist", &["m3u8", "m3u"]), library.dir()) else { return };
        let library = library.clone();
        let id = id.to_string();
        self.rt.spawn_blocking(move || {
            let result = match library.playlist_m3u8(&id) {
                Some(text) => std::fs::write(&path, text).map(|_| path).map_err(|e| e.to_string()),
                None => Err("the playlist is gone".to_string()),
            };
            let _ = slint::invoke_from_event_loop(move || {
                crate::BROWSER.with(|b| {
                    if let Some(b) = b.borrow().as_ref() {
                        match result {
                            Ok(path) => {
                                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                                b.toast(&format!("Exported to {name}"));
                            }
                            Err(e) => b.toast(&format!("Could not export: {e}")),
                        }
                    }
                });
            });
        });
    }

    /// "Delete" was confirmed: remove the file, then the song from the library and playlists.
    pub(crate) fn delete_confirmed(self: &Rc<Self>, send: &Rc<dyn Fn(Command)>) {
        let Some(uri) = self.pending_delete.borrow_mut().take() else { return };
        let Some(library) = self.library.clone() else { return };
        match std::fs::remove_file(&*uri) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                self.toast(&format!("Could not delete: {e}"));
                return;
            }
        }
        library.remove_file(&uri);
        library.playlists().remove_everywhere(&uri);
        // The deleted song may be the one playing: move on.
        if self.win.upgrade().is_some_and(|w| w.get_now_uri().as_str() == &*uri) {
            send(Command::Next);
        }
        let kind = self.st.borrow().kind.clone();
        self.start(kind);
        self.playlists_changed(false);
        get::GET.with(|g| {
            if let Some(g) = g.borrow().as_ref() {
                g.songs_changed();
            }
        });
        self.toast("Song deleted");
    }

    /// The command that plays a library row: inside a playlist, the playlist is the queue.
    pub(crate) fn play_command(&self, uri: &Arc<str>) -> Command {
        // A newer choice replaces a download that was waiting to start playing.
        get::GET.with(|g| {
            if let Some(g) = g.borrow().as_ref() {
                g.cancel_awaiting();
            }
        });
        match self.current_playlist() {
            Some(id) => Command::PlayFrom(Selection::Playlist { id, from: Some(uri.clone()) }),
            None => pick(self.win.upgrade().is_some_and(|w| w.get_autoplay()), uri),
        }
    }

    fn ask_name(&self, pending: Pending, title: &str, initial: &str, cover: Option<&str>) {
        *self.pending.borrow_mut() = Some(pending);
        *self.draft.borrow_mut() = Draft::Unchanged;
        if let Some(w) = self.win.upgrade() {
            w.set_name_title(title.into());
            w.set_name_initial(initial.into());
            w.set_name_cover(cover.and_then(|c| self.custom_cover(c)).unwrap_or_default());
            w.set_name_has_cover(cover.is_some());
            w.set_name_open(true);
        }
    }

    /// The tile in the dialog was clicked: choose an image file.
    pub(crate) fn pick_cover(self: &Rc<Self>) {
        // iOS has no file dialog: its photo picker answers later, through `cover_picked`.
        if let Some(pick) = crate::COVER_PICKER.with(|p| p.borrow().clone()) {
            pick(Box::new(|answer| {
                let _ = slint::invoke_from_event_loop(move || {
                    crate::BROWSER.with(|b| {
                        if let Some(b) = b.borrow().as_ref() {
                            b.cover_picked(answer);
                        }
                    });
                });
            }));
            return;
        }
        let Some(path) = crate::dialogs::pick_file("Choose a cover image", ("Images", &["png", "jpg", "jpeg"])) else {
            return;
        };
        self.cover_chosen(&path);
    }

    /// The photo library (or the file dialog) came back: a file to use, or a reason it did not.
    fn cover_picked(self: &Rc<Self>, answer: crate::CoverPick) {
        match answer {
            crate::CoverPick::Picked(path) => self.cover_chosen(&path),
            crate::CoverPick::Failed(e) => self.toast(&e),
            // Nothing to say about a cancel.
            crate::CoverPick::Cancelled => {}
        }
    }

    fn cover_chosen(self: &Rc<Self>, path: &std::path::Path) {
        match process_cover(path) {
            Ok((png, img)) => {
                *self.draft.borrow_mut() = Draft::Set(png);
                if let Some(w) = self.win.upgrade() {
                    w.set_name_cover(img);
                    w.set_name_has_cover(true);
                }
            }
            Err(e) => self.toast(&e),
        }
    }

    /// "Remove cover" in the dialog.
    pub(crate) fn clear_cover_draft(&self) {
        *self.draft.borrow_mut() = Draft::Clear;
        if let Some(w) = self.win.upgrade() {
            w.set_name_cover(slint::Image::default());
            w.set_name_has_cover(false);
        }
    }

    /// A chosen cover file as an image, kept so it is read once.
    pub(crate) fn custom_cover(&self, path: &str) -> Option<slint::Image> {
        if let Some(i) = self.custom.borrow().get(path) {
            return Some(i.clone());
        }
        let bytes = std::fs::read(path).ok()?;
        let d = meowtify_cache::decode_square(&bytes, COVER_FILE).ok()?;
        let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(d.width, d.height);
        buf.make_mut_bytes().copy_from_slice(&d.rgba);
        let img = slint::Image::from_rgba8(buf);
        self.custom.borrow_mut().insert(path.to_string(), img.clone());
        Some(img)
    }

    /// Writes the chosen cover next to the settings and points the playlist at it.
    fn apply_draft(&self, library: &meowtify_library::Library, id: &str) {
        let draft = std::mem::take(&mut *self.draft.borrow_mut());
        let new = match draft {
            Draft::Unchanged => return,
            Draft::Clear => None,
            Draft::Set(png) => {
                let dir = meowtify_library::config_dir().join("playlist-covers");
                let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
                let file = dir.join(format!("{id}-{stamp}.png"));
                if std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&file, png)).is_err() {
                    self.toast("Could not save the cover");
                    return;
                }
                Some(file.to_string_lossy().into_owned())
            }
        };
        if let Some(old) = library.playlists().set_cover(id, new) {
            self.custom.borrow_mut().remove(&old);
            let _ = std::fs::remove_file(old);
        }
    }

    pub(crate) fn name_submitted(self: &Rc<Self>, name: &str) {
        let Some(pending) = self.pending.borrow_mut().take() else { return };
        let Some(library) = self.library.clone() else { return };
        match pending {
            Pending::New { track } => {
                let id = library.playlists().create(name);
                let shown = library.playlists().get(&id).map(|p| p.name).unwrap_or_default();
                self.apply_draft(&library, &id);
                match track {
                    Some(uri) => {
                        library.playlists().add(&id, &uri);
                        self.toast(&format!("Added to {shown}"));
                    }
                    None => self.toast(&format!("Created {shown}")),
                }
            }
            Pending::Rename { id } => {
                library.playlists().rename(&id, name);
                self.apply_draft(&library, &id);
            }
        }
        self.playlists_changed(false);
    }
}

/// Side of a playlist cover in pixels.
const COVER: u32 = 88;
/// Side of a chosen cover image, stored as PNG. The sidebar shows it at 36 px.
const COVER_FILE: u32 = 176;

/// Cuts the largest centred square from an image file (PNG or JPEG), scales it down and
/// returns it as PNG contents and as an image for the dialog.
fn process_cover(path: &std::path::Path) -> Result<(Vec<u8>, slint::Image), String> {
    const MAX_BYTES: u64 = 25 * 1024 * 1024;
    let len = std::fs::metadata(path).map_err(|_| "Could not read that file".to_string())?.len();
    if len > MAX_BYTES {
        return Err("That image is too big (25 MB at most)".into());
    }
    let bytes = std::fs::read(path).map_err(|_| "Could not read that file".to_string())?;
    let d = meowtify_cache::decode_square(&bytes, COVER_FILE).map_err(|_| "Pick a PNG or JPEG image".to_string())?;
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, d.width, d.height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().map_err(|e| e.to_string())?;
        w.write_image_data(&d.rgba).map_err(|e| e.to_string())?;
    }
    let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(d.width, d.height);
    buf.make_mut_bytes().copy_from_slice(&d.rgba);
    Ok((out, slint::Image::from_rgba8(buf)))
}

/// A playlist cover from the covers of its songs: one image, or a 2 x 2 grid of four.
/// Fewer than four are shown as the first one alone. `Image::default()` when none.
pub(crate) fn collage(images: &[slint::Image]) -> slint::Image {
    use slint::{Rgba8Pixel, SharedPixelBuffer};
    let bufs: Vec<SharedPixelBuffer<Rgba8Pixel>> = images.iter().filter_map(|i| i.to_rgba8()).collect();
    if bufs.len() < 4 {
        return bufs.into_iter().next().map(slint::Image::from_rgba8).unwrap_or_default();
    }
    let mut out = SharedPixelBuffer::<Rgba8Pixel>::new(COVER, COVER);
    let half = COVER / 2;
    let px = out.make_mut_slice();
    for y in 0..COVER {
        for x in 0..COVER {
            let src = &bufs[((y / half) * 2 + x / half) as usize];
            let (sw, sh) = (src.width(), src.height());
            let sx = ((x % half) * sw / half).min(sw - 1);
            let sy = ((y % half) * sh / half).min(sh - 1);
            px[(y * COVER + x) as usize] = src.as_slice()[(sy * sw + sx) as usize];
        }
    }
    slint::Image::from_rgba8(out)
}

impl Browser {
    /// Sets the cover of every playlist from the song covers that are loaded, and
    /// starts loading the rest. Called when the playlists or a cover change.
    pub(crate) fn refresh_covers(&self) {
        let Some(library) = &self.library else { return };
        let ids = self.playlist_ids.borrow().clone();
        let mut art: Vec<slint::Image> = Vec::with_capacity(ids.len() + 1);
        art.push(slint::Image::default()); // All Songs has none
        let mut rows: std::collections::HashMap<String, Vec<usize>> = std::collections::HashMap::new();
        let mut wanted: Vec<String> = Vec::new();
        for (n, id) in ids.iter().enumerate() {
            let (img, vids) = self.playlist_cover(library, id);
            art.push(img);
            for v in vids {
                let users = rows.entry(v.clone()).or_default();
                if users.is_empty() {
                    wanted.push(v);
                }
                users.push(n + 1);
            }
        }
        // The cover service keeps the covers the sidebar shows, and loads the ones it lacks.
        self.thumbs.pin(wanted.iter().cloned());
        for v in &wanted {
            if self.thumbs.cached(v).is_none() && !self.cover_failed.borrow().contains(v) {
                self.thumbs.fetch(v, &meowtify_core::youtube_thumbnail(v));
            }
        }
        *self.cover_rows.borrow_mut() = rows;
        self.source_art.set_vec(art);
    }

    /// The cover of one playlist, and the video ids of the song covers it is made from (none
    /// for a cover the user chose).
    fn playlist_cover(&self, library: &meowtify_library::Library, id: &str) -> (slint::Image, Vec<String>) {
        // A cover the user chose wins over the automatic one.
        if let Some(img) = library.playlists().get(id).and_then(|p| p.cover).and_then(|c| self.custom_cover(&c)) {
            return (img, Vec::new());
        }
        let vids = self.cover_ids(library, id);
        let loaded: Vec<slint::Image> = vids.iter().filter_map(|v| self.thumbs.cached(v)).collect();
        // With four songs, wait for all four, so the cover does not flip while loading.
        let ready = if vids.len() >= 4 && loaded.len() < 4 { &loaded[..loaded.len().min(1)] } else { &loaded[..] };
        (collage(ready), vids)
    }

    /// Video ids of up to four songs of a playlist that have a known cover.
    fn cover_ids(&self, library: &meowtify_library::Library, id: &str) -> Vec<String> {
        let Some(p) = library.playlists().get(id) else { return Vec::new() };
        p.tracks.iter().filter_map(|t| meowtify_core::youtube_id(t).map(str::to_string)).take(4).collect()
    }

    /// A song cover arrived, or failed. Only the playlists that use it get a new cover.
    pub(crate) fn cover_ready(&self, video_id: &str, img: Option<&slint::Image>) {
        // Every thumbnail of the app arrives here; most are not a playlist's.
        let Some(rows) = self.cover_rows.borrow().get(video_id).cloned() else { return };
        if img.is_none() {
            let mut failed = self.cover_failed.borrow_mut();
            if failed.len() >= 256 {
                failed.clear();
            }
            failed.insert(video_id.to_string());
            return;
        }
        let Some(library) = &self.library else { return };
        for row in rows {
            let Some(id) = self.playlist_ids.borrow().get(row - 1).cloned() else { continue };
            let (cover, _) = self.playlist_cover(library, &id);
            slint::Model::set_row_data(&*self.source_art, row, cover);
        }
    }
}
