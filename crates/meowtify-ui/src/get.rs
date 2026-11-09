//! "Get music" pages: provider search and the download list.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use meowtify_core::Command;
use meowtify_library::Library;
use meowtify_provider::{DownloadManager, Item, JobState, ProviderError};
use slint::{ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel};

use crate::thumbs::Thumbs;

use crate::{JobRow, MainWindow, QueueRow, ResultRow, BROWSER};

const RESULTS: u32 = 15;
/// A search never asks the provider for more than this many items.
const MAX_RESULTS: u32 = 100;
const SAVED: &str = "Saved";
/// Pause after the last keystroke before searching.
const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(250);
const CACHED_SEARCHES: usize = 16;
/// Pause after a keystroke before asking for suggestions.
const SUGGEST_DELAY: std::time::Duration = std::time::Duration::from_millis(70);
const MAX_SUGGESTIONS: usize = 6;
/// A saved song stays in the sidebar queue this long, then slides out over `QUEUE_LEAVE`.
const QUEUE_SAVED_HOLD: std::time::Duration = std::time::Duration::from_millis(900);
const QUEUE_LEAVE: std::time::Duration = std::time::Duration::from_millis(400);

/// Brings `model` in line with `rows` (id, data) with the fewest changes: rows that stay
/// keep their element, so they do not replay their enter animation.
pub(crate) fn sync_rows<T: Clone + PartialEq + 'static>(model: &VecModel<T>, ids: &mut Vec<u64>, rows: Vec<(u64, T)>) {
    let mut i = 0;
    for (id, row) in rows {
        if ids.get(i) == Some(&id) {
            if model.row_data(i).as_ref() != Some(&row) {
                model.set_row_data(i, row);
            }
        } else if let Some(off) = ids[i..].iter().position(|x| *x == id) {
            for _ in 0..off {
                model.remove(i);
                ids.remove(i);
            }
            if model.row_data(i).as_ref() != Some(&row) {
                model.set_row_data(i, row);
            }
        } else {
            model.insert(i, row);
            ids.insert(i, id);
        }
        i += 1;
    }
    while ids.len() > i {
        model.remove(ids.len() - 1);
        ids.pop();
    }
}

fn starts_with_ignore_case(s: &str, prefix: &str) -> bool {
    let mut it = s.chars();
    prefix.chars().all(|p| it.next().is_some_and(|c| c.to_lowercase().eq(p.to_lowercase())))
}

/// What is left of `suggestion` after what was typed, when it starts with it.
fn ghost_for(typed: &str, suggestion: &str) -> String {
    if typed.is_empty() || !starts_with_ignore_case(suggestion, typed) {
        return String::new();
    }
    suggestion.chars().skip(typed.chars().count()).collect()
}

#[cfg(test)]
mod tests {
    use super::{ghost_for, starts_with_ignore_case, sync_rows};
    use slint::{Model, VecModel};

    #[test]
    fn sync_rows_inserts_removes_and_updates_in_place() {
        let m = VecModel::from(vec!["a", "b", "c"]);
        let mut ids = vec![1u64, 2, 3];
        // b leaves, d arrives at the end, c changes.
        sync_rows(&m, &mut ids, vec![(1, "a"), (3, "c2"), (4, "d")]);
        assert_eq!(m.iter().collect::<Vec<_>>(), ["a", "c2", "d"]);
        assert_eq!(ids, [1, 3, 4]);
        // A new one in front, and everything else dropped.
        sync_rows(&m, &mut ids, vec![(9, "z"), (4, "d")]);
        assert_eq!(m.iter().collect::<Vec<_>>(), ["z", "d"]);
        assert_eq!(ids, [9, 4]);
        sync_rows(&m, &mut ids, vec![]);
        assert_eq!(m.row_count(), 0);
    }

    #[test]
    fn ghost_is_the_untyped_tail() {
        assert_eq!(ghost_for("tsu", "tsubi club"), "bi club");
        assert_eq!(ghost_for("TSU", "tsubi club"), "bi club");
        assert_eq!(ghost_for("tsubi club", "tsubi club"), "");
        assert_eq!(ghost_for("abc", "tsubi club"), "");
        assert_eq!(ghost_for("", "tsubi club"), "");
    }

    #[test]
    fn prefix_match_ignores_case_and_handles_short_text() {
        assert!(starts_with_ignore_case("Hello", "he"));
        assert!(!starts_with_ignore_case("he", "hello"));
        assert!(starts_with_ignore_case("( ╥﹏╥ )", "( ╥"));
    }
}

thread_local! {
    pub(crate) static GET: RefCell<Option<Rc<Get>>> = const { RefCell::new(None) };
}

/// Returns the `on_change` callback for [`DownloadManager::start`]. It may be
/// called from any thread. Bursts of calls turn into one UI refresh.
pub fn downloads_notifier() -> impl Fn() + Send + Sync + 'static {
    let pending = Arc::new(AtomicBool::new(false));
    move || {
        if !pending.swap(true, Ordering::AcqRel) {
            let pending = pending.clone();
            let _ = slint::invoke_from_event_loop(move || {
                pending.store(false, Ordering::Release);
                GET.with(|g| {
                    if let Some(g) = g.borrow().as_ref() {
                        g.refresh_jobs();
                    }
                });
            });
        }
    }
}

pub(crate) struct Get {
    win: slint::Weak<MainWindow>,
    manager: DownloadManager,
    library: Option<Arc<Library>>,
    rt: tokio::runtime::Handle,
    send: Rc<dyn Fn(Command)>,
    thumbs: Rc<Thumbs>,
    placeholder: Image,
    /// Covers that have arrived, by item id.
    /// Video id of a song the user clicked that should play once it is in the library.
    awaiting: RefCell<Option<String>>,
    /// A folder rescan was started to find a saved file; `library_ready` ends the wait.
    rescanning: Cell<bool>,
    /// Streamed songs listed in the library while their download runs, by job id. They are
    /// provisional: a failed or cancelled job takes its entry out again.
    provisional: RefCell<HashMap<u64, std::path::PathBuf>>,
    items: RefCell<Vec<Item>>,
    results: Rc<VecModel<ResultRow>>,
    jobs: Rc<VecModel<JobRow>>,
    job_ids: RefCell<Vec<u64>>,
    /// Sidebar queue: active downloads, then saved songs on their way out.
    queue: Rc<VecModel<QueueRow>>,
    queue_ids: RefCell<Vec<u64>>,
    /// When each finished job was first seen as finished.
    finished: RefCell<HashMap<u64, std::time::Instant>>,
    queue_timer: slint::Timer,
    /// Bumped on every search so a late answer to an older one is dropped.
    gen: Cell<u64>,
    /// The running search. Aborting it drops the future, which kills yt-dlp.
    task: RefCell<Option<tokio::task::JoinHandle<()>>>,
    /// Waits a moment after the last keystroke, then searches.
    debounce: slint::Timer,
    /// Recent searches, newest first, so repeating one is instant.
    cache: RefCell<Vec<(String, Vec<Item>)>>,
    /// Cache key of the search in flight.
    pending_key: RefCell<String>,
    /// The text of the search that fills the list, and how many items it asked for.
    last_query: RefCell<String>,
    limit: Cell<u32>,
    /// The provider has no more results for `last_query`.
    exhausted: Cell<bool>,
    /// Autocomplete: the fetch in flight, the pause before it, answers by lowercase
    /// text, and the last answer (kept while typing so the list does not flicker).
    suggest_task: RefCell<Option<tokio::task::JoinHandle<()>>>,
    suggest_timer: slint::Timer,
    suggest_cache: RefCell<HashMap<String, Vec<String>>>,
    last_remote: RefCell<Vec<String>>,
    suggest_gen: Cell<u64>,
    /// Results already looked up ahead of a click, by video id.
    prefetched: RefCell<std::collections::HashSet<String>>,
    hover_timer: slint::Timer,
    top_timer: slint::Timer,
    /// A `meowtify://play?q=` link asked for a song: play the first result of the next search.
    play_first: Cell<bool>,
    /// Jobs whose file is already in the library.
    added: RefCell<std::collections::HashSet<u64>>,
    /// Video ids of songs being saved that go to the queue as soon as they are in the library.
    queue_after: RefCell<Vec<(String, u32)>>,
}

fn duration_text(secs: Option<u32>) -> String {
    match secs {
        Some(s) => format!("{}:{:02}", s / 60, s % 60),
        None => String::new(),
    }
}

impl Get {
    pub(crate) fn new(
        win: &MainWindow,
        manager: DownloadManager,
        library: Option<Arc<Library>>,
        thumbs: Rc<Thumbs>,
        rt: tokio::runtime::Handle,
        send: Rc<dyn Fn(Command)>,
    ) -> Rc<Get> {
        let results = Rc::new(VecModel::default());
        let jobs = Rc::new(VecModel::default());
        win.set_results(ModelRc::from(results.clone()));
        win.set_jobs(ModelRc::from(jobs.clone()));
        let queue = Rc::new(VecModel::default());
        win.set_queue(ModelRc::from(queue.clone()));
        let mut ph = SharedPixelBuffer::<Rgba8Pixel>::new(8, 8);
        ph.make_mut_slice().fill(Rgba8Pixel { r: 70, g: 70, b: 74, a: 255 });
        let get = Rc::new(Get {
            win: win.as_weak(),
            thumbs: thumbs.clone(),
            placeholder: Image::from_rgba8(ph),
            manager,
            library,
            rt,
            send,
            awaiting: RefCell::new(None),
            rescanning: Cell::new(false),
            provisional: RefCell::new(HashMap::new()),
            items: RefCell::new(Vec::new()),
            results,
            jobs,
            job_ids: RefCell::new(Vec::new()),
            queue,
            queue_ids: RefCell::new(Vec::new()),
            finished: RefCell::new(HashMap::new()),
            queue_timer: slint::Timer::default(),
            gen: Cell::new(0),
            task: RefCell::new(None),
            debounce: slint::Timer::default(),
            cache: RefCell::new(Vec::new()),
            pending_key: RefCell::new(String::new()),
            last_query: RefCell::new(String::new()),
            limit: Cell::new(RESULTS),
            exhausted: Cell::new(true),
            suggest_task: RefCell::new(None),
            suggest_timer: slint::Timer::default(),
            suggest_cache: RefCell::new(HashMap::new()),
            last_remote: RefCell::new(Vec::new()),
            suggest_gen: Cell::new(0),
            prefetched: RefCell::new(std::collections::HashSet::new()),
            hover_timer: slint::Timer::default(),
            top_timer: slint::Timer::default(),
            play_first: Cell::new(false),
            added: RefCell::new(std::collections::HashSet::new()),
            queue_after: RefCell::new(Vec::new()),
        });
        {
            let weak = Rc::downgrade(&get);
            thumbs.listen(move |id, img| {
                if let (Some(g), Some(img)) = (weak.upgrade(), img) {
                    g.thumb_ready(id, img);
                }
            });
        }
        get
    }

    /// Puts a cover that arrived into its result row.
    fn thumb_ready(&self, id: &str, img: &Image) {
        // Every thumbnail of the app arrives here; keep only the ones of the shown results.
        let Some(i) = self.items.borrow().iter().position(|it| it.id == id) else { return };
        if let Some(mut row) = self.results.row_data(i) {
            row.art = img.clone();
            self.results.set_row_data(i, row);
        }
    }

    /// The user chose something else to play: a download that waits to start playing must
    /// not start later and replace that choice.
    pub(crate) fn cancel_awaiting(&self) {
        *self.awaiting.borrow_mut() = None;
        self.rescanning.set(false);
        if let Some(w) = self.win.upgrade() {
            w.set_status(SharedString::default());
        }
    }

    /// Enter pressed: search at once.
    pub(crate) fn search_now(self: &Rc<Self>, text: &str) {
        self.debounce.stop();
        self.suggest_cancel();
        self.search(text);
    }

    /// The search box changed. Searches after a short pause, so results follow typing.
    /// Pasted links wait for Enter, because half a link is an error.
    pub(crate) fn query_edited(self: &Rc<Self>, text: &str) {
        self.debounce.stop();
        let text = text.trim().to_string();
        // Any edit makes the search in flight stale, also when no new search starts (a
        // half-typed link, one letter): its answer is for text that is gone.
        self.gen.set(self.gen.get() + 1);
        if let Some(t) = self.task.borrow_mut().take() {
            t.abort();
        }
        if let Some(w) = self.win.upgrade() {
            w.set_more_busy(false);
        }
        if text.is_empty() {
            self.cancel_search();
            return;
        }
        if text.starts_with("http") || text.chars().count() < 2 {
            if let Some(w) = self.win.upgrade() {
                w.set_yt_busy(false);
            }
            return;
        }
        let weak = Rc::downgrade(self);
        self.debounce.start(slint::TimerMode::SingleShot, DEBOUNCE, move || {
            if let Some(g) = weak.upgrade() {
                g.search(&text);
            }
        });
    }

    fn suggest_cancel(&self) {
        self.suggest_timer.stop();
        self.suggest_gen.set(self.suggest_gen.get() + 1);
        if let Some(t) = self.suggest_task.borrow_mut().take() {
            t.abort();
        }
    }

    /// Songs in the library whose title or artist starts with `typed`. At most two.
    fn local_matches(&self, typed: &str) -> Vec<String> {
        let Some(library) = &self.library else { return Vec::new() };
        let mut out: Vec<String> = Vec::new();
        for t in library.tracks().iter() {
            for cand in std::iter::once(t.name.as_ref()).chain(t.artists.first().map(|a| a.name.as_ref())) {
                if starts_with_ignore_case(cand, typed) && !out.iter().any(|o| o == cand) {
                    out.push(cand.to_string());
                }
            }
            if out.len() >= 2 {
                break;
            }
        }
        out
    }

    /// The search box changed: show what is known at once, then ask the provider.
    pub(crate) fn suggest_request(self: &Rc<Self>, text: &str) {
        self.suggest_cancel();
        let typed = text.to_string();
        let query = typed.trim().to_string();
        if query.is_empty() || query.starts_with("http") {
            self.last_remote.borrow_mut().clear();
            self.show_suggestions(&typed, Vec::new());
            return;
        }
        let key = query.to_lowercase();
        let cached = self.suggest_cache.borrow().get(&key).cloned();
        if let Some(remote) = cached {
            *self.last_remote.borrow_mut() = remote.clone();
            self.show_suggestions(&typed, self.merge_suggestions(&query, &remote));
            return;
        }
        // Until the answer comes, keep earlier suggestions that still fit.
        let still: Vec<String> =
            self.last_remote.borrow().iter().filter(|s| starts_with_ignore_case(s, &query)).cloned().collect();
        self.show_suggestions(&typed, self.merge_suggestions(&query, &still));

        let gen = self.suggest_gen.get();
        let weak = Rc::downgrade(self);
        let rt = self.rt.clone();
        self.suggest_timer.start(slint::TimerMode::SingleShot, SUGGEST_DELAY, move || {
            let Some(g) = weak.upgrade() else { return };
            let (typed, query, key) = (typed.clone(), query.clone(), key.clone());
            let handle = rt.spawn(async move {
                let remote = meowtify_provider::suggest(&query).await.unwrap_or_default();
                let _ = slint::invoke_from_event_loop(move || {
                    GET.with(|g| {
                        if let Some(g) = g.borrow().as_ref() {
                            if g.suggest_gen.get() == gen {
                                let mut cache = g.suggest_cache.borrow_mut();
                                if cache.len() >= 64 {
                                    cache.clear();
                                }
                                cache.insert(key, remote.clone());
                                drop(cache);
                                *g.last_remote.borrow_mut() = remote.clone();
                                g.show_suggestions(&typed, g.merge_suggestions(&query, &remote));
                            }
                        }
                    });
                });
            });
            *g.suggest_task.borrow_mut() = Some(handle);
        });
    }

    /// Library matches first, then the provider's, without repeats.
    fn merge_suggestions(&self, query: &str, remote: &[String]) -> Vec<String> {
        let mut out = self.local_matches(query);
        for r in remote {
            if !out.iter().any(|o| o.eq_ignore_ascii_case(r)) {
                out.push(r.clone());
            }
        }
        out.truncate(MAX_SUGGESTIONS);
        out
    }

    fn show_suggestions(&self, typed: &str, list: Vec<String>) {
        let Some(w) = self.win.upgrade() else { return };
        let ghost = list.first().map(|f| ghost_for(typed, f)).unwrap_or_default();
        let rows: Vec<SharedString> = list.iter().map(|s| s.as_str().into()).collect();
        w.set_suggestions(ModelRc::new(VecModel::from(rows)));
        w.set_suggest_index(-1);
        w.set_ghost(ghost.into());
    }

    /// Stops the running search and clears the busy state.
    fn cancel_search(&self) {
        self.gen.set(self.gen.get() + 1);
        if let Some(t) = self.task.borrow_mut().take() {
            t.abort();
        }
        if let Some(w) = self.win.upgrade() {
            w.set_yt_busy(false);
            w.set_more_busy(false);
            w.set_yt_status(SharedString::default());
        }
    }

    /// Searches for `text` and plays the first result. A Siri Shortcut opens
    /// `meowtify://play?q=<title>` to ask for this.
    pub(crate) fn play_query(self: &Rc<Self>, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        if let Some(w) = self.win.upgrade() {
            w.set_view(0);
        }
        // Set before the search: a cached answer arrives at once.
        self.play_first.set(true);
        self.search_with(text);
    }

    /// Any other search drops a pending "play the first result".
    pub(crate) fn search(self: &Rc<Self>, text: &str) {
        self.play_first.set(false);
        self.search_with(text);
    }

    fn search_with(self: &Rc<Self>, text: &str) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        let gen = self.gen.get() + 1;
        self.gen.set(gen);
        self.top_timer.stop();
        if let Some(t) = self.task.borrow_mut().take() {
            t.abort();
        }
        // The rows that were hovered are about to go; lookups still waiting for them are wasted.
        self.manager.provider().cancel_prefetch();
        let key = text.to_lowercase();
        if let Some(w) = self.win.upgrade() {
            w.set_show_results(true);
            w.set_more_busy(false);
            w.set_yt_status(SharedString::default());
        }
        *self.last_query.borrow_mut() = text.clone();
        self.limit.set(RESULTS);
        let hit = {
            let mut c = self.cache.borrow_mut();
            c.iter().position(|(k, _)| *k == key).map(|i| c.remove(i))
        };
        if let Some((k, items)) = hit {
            self.cache.borrow_mut().insert(0, (k, items.clone()));
            // A list that was grown by scrolling comes back whole; the next step continues from it.
            self.limit.set(RESULTS.max(items.len() as u32));
            self.show_results(gen, Ok(items));
            return;
        }
        if let Some(w) = self.win.upgrade() {
            w.set_yt_busy(true);
        }
        *self.pending_key.borrow_mut() = key;
        let provider = self.manager.provider().clone();
        let handle = self.rt.spawn(async move {
            let result = if text.starts_with("http") {
                provider.resolve(&text).await
            } else {
                provider.search(&text, RESULTS).await
            };
            let _ = slint::invoke_from_event_loop(move || {
                GET.with(|g| {
                    if let Some(g) = g.borrow().as_ref() {
                        g.show_results(gen, result);
                    }
                });
            });
        });
        *self.task.borrow_mut() = Some(handle);
    }

    fn show_results(&self, gen: u64, result: Result<Vec<Item>, ProviderError>) {
        if gen != self.gen.get() {
            return;
        }
        let Some(w) = self.win.upgrade() else { return };
        w.set_yt_busy(false);
        match result {
            Ok(items) => {
                if !items.is_empty() {
                    let key = self.pending_key.borrow().clone();
                    let mut c = self.cache.borrow_mut();
                    if !key.is_empty() && !c.iter().any(|(k, _)| *k == key) {
                        c.insert(0, (key, items.clone()));
                        c.truncate(CACHED_SEARCHES);
                    }
                }
                if items.is_empty() {
                    w.set_yt_status("Nothing found.".into());
                }
                // A pasted link gives its whole list at once; a short answer means there is no more.
                let is_link = self.last_query.borrow().starts_with("http");
                self.exhausted.set(is_link || items.len() < self.limit.get() as usize);
                let jobs = self.manager.snapshot();
                let rows: Vec<ResultRow> = items.iter().map(|it| self.result_row(it, &jobs)).collect();
                self.results.set_vec(rows);
                for it in &items {
                    if let Some(url) = &it.thumbnail {
                        self.thumbs.fetch(&it.id, url);
                    }
                }
                *self.items.borrow_mut() = items;
                if self.play_first.replace(false) && !self.items.borrow().is_empty() {
                    if let Some(g) = GET.with(|g| g.borrow().clone()) {
                        g.play(0);
                    }
                }
                // The first result is the likeliest pick. Look it up once the list has stayed a moment.
                if !self.items.borrow().is_empty() {
                    self.top_timer.start(slint::TimerMode::SingleShot, std::time::Duration::from_millis(700), || {
                        GET.with(|g| {
                            if let Some(g) = g.borrow().as_ref() {
                                g.prefetch(0);
                            }
                        });
                    });
                }
                #[cfg(any(debug_assertions, feature = "dev"))]
                if let Some(n) = crate::dev::autoplay_pick() {
                    if let Some(g) = GET.with(|g| g.borrow().clone()) {
                        g.play(n);
                    }
                }
            }
            Err(e) => {
                self.results.set_vec(Vec::new());
                self.items.borrow_mut().clear();
                self.exhausted.set(true);
                w.set_yt_status(e.to_string().into());
            }
        }
    }

    /// The row a result is shown as.
    fn result_row(&self, it: &Item, jobs: &[meowtify_provider::Job]) -> ResultRow {
        let mut sub = it.artist.clone();
        let d = duration_text(it.duration_secs);
        if !d.is_empty() {
            if !sub.is_empty() {
                sub.push_str(" · ");
            }
            sub.push_str(&d);
        }
        ResultRow {
            key: it.id.as_str().into(),
            title: it.title.as_str().into(),
            subtitle: sub.into(),
            state: self.state_for(it, jobs).into(),
            art: self.thumbs.cached(&it.id).unwrap_or_else(|| self.placeholder.clone()),
        }
    }

    /// The list was scrolled near its end: ask the provider for the next step of results.
    pub(crate) fn load_more(self: &Rc<Self>) {
        let Some(w) = self.win.upgrade() else { return };
        if self.exhausted.get() || w.get_yt_busy() || w.get_more_busy() || !w.get_show_results() {
            return;
        }
        let text = self.last_query.borrow().clone();
        let limit = (self.limit.get() + RESULTS).min(MAX_RESULTS);
        if text.is_empty() || self.items.borrow().is_empty() || limit <= self.limit.get() {
            self.exhausted.set(true);
            return;
        }
        let gen = self.gen.get();
        w.set_more_busy(true);
        let provider = self.manager.provider().clone();
        let handle = self.rt.spawn(async move {
            let result = provider.search(&text, limit).await;
            let _ = slint::invoke_from_event_loop(move || {
                GET.with(|g| {
                    if let Some(g) = g.borrow().as_ref() {
                        g.more_results(gen, limit, result);
                    }
                });
            });
        });
        *self.task.borrow_mut() = Some(handle);
    }

    /// The answer to a bigger search: the results not shown yet go to the end of the list, so
    /// the rows on screen do not move.
    fn more_results(&self, gen: u64, limit: u32, result: Result<Vec<Item>, ProviderError>) {
        if gen != self.gen.get() {
            return;
        }
        let Some(w) = self.win.upgrade() else { return };
        w.set_more_busy(false);
        let all = match result {
            Ok(all) => all,
            Err(e) => {
                self.exhausted.set(true);
                w.set_yt_status(e.to_string().into());
                return;
            }
        };
        self.limit.set(limit);
        if all.len() < limit as usize {
            self.exhausted.set(true);
        }
        let fresh: Vec<Item> = {
            let have: std::collections::HashSet<String> = self.items.borrow().iter().map(|i| i.id.clone()).collect();
            all.into_iter().filter(|it| !have.contains(&it.id)).collect()
        };
        if fresh.is_empty() {
            self.exhausted.set(true);
            return;
        }
        let jobs = self.manager.snapshot();
        for it in &fresh {
            self.results.push(self.result_row(it, &jobs));
            if let Some(url) = &it.thumbnail {
                self.thumbs.fetch(&it.id, url);
            }
        }
        self.items.borrow_mut().extend(fresh);
        let key = self.last_query.borrow().to_lowercase();
        let mut c = self.cache.borrow_mut();
        if let Some(entry) = c.iter_mut().find(|(k, _)| *k == key) {
            entry.1 = self.items.borrow().clone();
        }
    }

    /// Short text for a result row: what is happening to that song.
    fn state_for(&self, item: &Item, jobs: &[meowtify_provider::Job]) -> String {
        if let Some(j) = jobs.iter().rev().find(|j| j.item.provider == item.provider && j.item.id == item.id) {
            match &j.state {
                JobState::Queued => return "Waiting".into(),
                JobState::Running(p) if *p < 1.0 => return "Starting\u{2026}".into(),
                JobState::Running(p) => return format!("{p:.0}%"),
                JobState::Failed(_) => return "Failed".into(),
                JobState::Done(_) | JobState::Cancelled => {}
            }
        }
        match &self.library {
            Some(l) if l.find_by_video_id(&item.id).is_some() => "In library".into(),
            _ => String::new(),
        }
    }

    /// Updates the status text of every result row.
    fn update_result_states(&self) {
        self.update_result_states_from(&self.manager.snapshot());
    }

    /// Like [`update_result_states`](Self::update_result_states), for a caller that holds a
    /// snapshot already, so one refresh reads the manager once.
    fn update_result_states_from(&self, jobs: &[meowtify_provider::Job]) {
        let items = self.items.borrow();
        for (i, item) in items.iter().enumerate() {
            let state = self.state_for(item, jobs);
            if let Some(mut row) = self.results.row_data(i) {
                if row.state.as_str() != state {
                    row.state = state.into();
                    self.results.set_row_data(i, row);
                }
            }
        }
    }

    /// The command that plays a song the user picked. With autoplay on the song is the
    /// whole queue, so similar songs can follow it when it ends.
    fn pick(&self, uri: &Arc<str>) -> Command {
        crate::menu::pick(self.win.upgrade().is_some_and(|w| w.get_autoplay()), uri)
    }

    /// The user clicked a result: play it now if it is in the library, otherwise
    /// download it first and play it as soon as it is saved.
    pub(crate) fn play(self: &Rc<Self>, idx: usize) {
        let Some(w) = self.win.upgrade() else { return };
        let Some(item) = self.items.borrow().get(idx).cloned() else { return };
        let Some(library) = &self.library else { return };

        if let Some(track) = library.find_by_video_id(&item.id) {
            *self.awaiting.borrow_mut() = None;
            w.set_yt_status(SharedString::default());
            (self.send)(self.pick(&track.uri));
            return;
        }
        let Some(dir) = library.dir() else {
            w.set_yt_status("Choose a music folder in Settings first.".into());
            w.set_view(1);
            return;
        };
        w.set_yt_status(SharedString::default());
        w.set_status(format!("Downloading \u{201c}{}\u{201d}\u{2026}", item.title).into());
        *self.awaiting.borrow_mut() = Some(item.id.clone());
        // Opt-in: with streaming on, the song starts as soon as the first part is saved.
        if w.get_stream_playback() {
            self.manager.enqueue_stream(item, dir);
        } else {
            self.manager.enqueue_next(item, dir);
        }
        self.update_result_states();
    }

    /// A streamed song is saved: write its tags in the background, then reload its entry.
    /// The provider swaps the file in when nothing is reading it, so this can take as long
    /// as the song plays.
    fn finish_stream(&self, item: Item, path: std::path::PathBuf) {
        let provider = self.manager.provider().clone();
        let library = self.library.clone();
        self.rt.spawn(async move {
            if provider.finish(&item, &path).await.is_err() {
                return; // the song stays untagged; playing it is not affected
            }
            // Reading the tags again is disk work: do it here, not on the UI thread.
            if let Some(l) = library.clone() {
                let path = path.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    l.remove_file(&path.to_string_lossy());
                    l.add_file(&path);
                })
                .await;
            }
            let _ = slint::invoke_from_event_loop(move || {
                BROWSER.with(|b| {
                    if let Some(b) = b.borrow().as_ref() {
                        b.library_changed();
                    }
                });
            });
        });
    }

    /// Songs were removed from the library: results saved before show "Install" again.
    pub(crate) fn songs_changed(&self) {
        self.prefetched.borrow_mut().clear();
        self.update_result_states();
    }

    /// The pointer rests on result `idx`: look it up now, so a click starts at once.
    pub(crate) fn hovered(self: &Rc<Self>, idx: usize) {
        let weak = Rc::downgrade(self);
        self.hover_timer.start(slint::TimerMode::SingleShot, std::time::Duration::from_millis(120), move || {
            if let Some(g) = weak.upgrade() {
                g.prefetch(idx);
            }
        });
    }

    fn prefetch(&self, idx: usize) {
        let Some(item) = self.items.borrow().get(idx).cloned() else { return };
        if self.library.as_ref().is_some_and(|l| l.find_by_video_id(&item.id).is_some()) {
            return;
        }
        {
            let mut done = self.prefetched.borrow_mut();
            if done.len() >= 64 {
                done.clear();
            }
            if !done.insert(item.id.clone()) {
                return;
            }
        }
        let provider = self.manager.provider().clone();
        self.rt.spawn(async move { provider.prefetch(&item).await });
    }

    /// Provider id of result `idx`.
    pub(crate) fn item_id(&self, idx: usize) -> Option<String> {
        self.items.borrow().get(idx).map(|it| it.id.clone())
    }

    /// Where the result with this id is now.
    pub(crate) fn item_index(&self, id: &str) -> Option<usize> {
        self.items.borrow().iter().position(|it| it.id == id)
    }

    /// File path of the result with this id when it is in the library.
    pub(crate) fn uri_of_id(&self, id: &str) -> Option<Arc<str>> {
        self.library.as_ref()?.find_by_video_id(id).map(|t| t.uri)
    }

    /// File path of result `idx` when it is already in the library.
    pub(crate) fn result_uri(&self, idx: usize) -> Option<Arc<str>> {
        let id = self.items.borrow().get(idx)?.id.clone();
        self.library.as_ref()?.find_by_video_id(&id).map(|t| t.uri)
    }

    /// The install button: download a result without playing it.
    pub(crate) fn download(self: &Rc<Self>, idx: usize) {
        let Some(w) = self.win.upgrade() else { return };
        let Some(item) = self.items.borrow().get(idx).cloned() else { return };
        self.download_item(&w, item);
    }

    fn download_item(&self, w: &MainWindow, item: Item) {
        let Some(library) = &self.library else { return };
        if library.find_by_video_id(&item.id).is_some() {
            return;
        }
        let Some(dir) = library.dir() else {
            w.set_yt_status("Choose a music folder in Settings first.".into());
            w.set_view(1);
            return;
        };
        w.set_yt_status(SharedString::default());
        self.manager.enqueue(item, dir);
        self.update_result_states();
    }

    /// Songs saved for the queue that the library has now go to the queue.
    fn queue_saved(&self) {
        let Some(library) = &self.library else { return };
        let ready: Vec<(String, Arc<str>, u32)> = self
            .queue_after
            .borrow()
            .iter()
            .filter_map(|(id, by)| library.find_by_video_id(id).map(|t| (id.clone(), t.uri.clone(), *by)))
            .collect();
        for (id, uri, by) in ready {
            self.queue_after.borrow_mut().retain(|(x, _)| *x != id);
            crate::jam::note_adder(&uri, by);
            (self.send)(Command::Enqueue { uri, place: meowtify_core::Place::Queue });
            if let Some(w) = self.win.upgrade() {
                w.set_status("Added the suggested song to the queue".into());
            }
        }
    }

    /// Finds the best match for `text` and puts it in the queue, saving it first when it is not
    /// in the library yet. For a song a jam guest suggested.
    pub(crate) fn queue_text(self: &Rc<Self>, text: &str, by: u32) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        let provider = self.manager.provider().clone();
        self.rt.spawn(async move {
            let result = if text.starts_with("http") { provider.resolve(&text).await } else { provider.search(&text, 1).await };
            let _ = slint::invoke_from_event_loop(move || {
                GET.with(|g| {
                    if let Some(g) = g.borrow().as_ref() {
                        g.queue_found(result, by);
                    }
                });
            });
        });
    }

    fn queue_found(&self, result: Result<Vec<Item>, ProviderError>, by: u32) {
        let Some(w) = self.win.upgrade() else { return };
        let Some(item) = result.ok().and_then(|v| v.into_iter().next()) else {
            w.set_status("Could not find that suggestion.".into());
            return;
        };
        let Some(library) = &self.library else { return };
        if let Some(track) = library.find_by_video_id(&item.id) {
            crate::jam::note_adder(&track.uri, by);
            (self.send)(Command::Enqueue { uri: track.uri.clone(), place: meowtify_core::Place::Queue });
            w.set_status(format!("Added \u{201c}{}\u{201d} to the queue", item.title).into());
            return;
        }
        let Some(dir) = library.dir() else {
            w.set_yt_status("Choose a music folder in Settings first.".into());
            w.set_view(1);
            return;
        };
        w.set_status(format!("Saving \u{201c}{}\u{201d} for the queue\u{2026}", item.title).into());
        {
            let mut after = self.queue_after.borrow_mut();
            if after.len() >= 8 {
                after.remove(0);
            }
            after.push((item.id.clone(), by));
        }
        self.manager.enqueue_next(item, dir);
        self.update_result_states();
    }

    /// Called after every library rescan. Starts the song the user is waiting for.
    pub(crate) fn library_ready(&self) {
        self.rescanning.set(false);
        self.queue_saved();
        let Some(id) = self.awaiting.borrow().clone() else { return };
        let Some(library) = &self.library else { return };
        if let Some(track) = library.find_by_video_id(&id) {
            *self.awaiting.borrow_mut() = None;
            (self.send)(self.pick(&track.uri));
            self.update_result_states();
        } else if self.manager.snapshot().iter().any(|j| j.item.id == id && matches!(j.state, JobState::Done(_))) {
            // Saved, but the scan could not read it.
            *self.awaiting.borrow_mut() = None;
            if let Some(w) = self.win.upgrade() {
                w.set_status("Downloaded, but the file could not be read as music.".into());
            }
        }
    }

    pub(crate) fn cancel(&self, idx: usize) {
        if let Some(id) = self.job_ids.borrow().get(idx).copied() {
            self.manager.cancel(id);
        }
    }

    pub(crate) fn clear_finished(&self) {
        self.manager.clear_finished();
    }

    /// Updates the sidebar queue. A saved song shows a full square for a moment, then
    /// slides out. Failed ones stay until they are cleared from the Downloads page.
    fn refresh_queue(&self, snapshot: &[meowtify_provider::Job]) {
        let now = std::time::Instant::now();
        let mut rows: Vec<(u64, QueueRow)> = Vec::with_capacity(snapshot.len());
        let mut animating = false;
        {
            let mut fin = self.finished.borrow_mut();
            fin.retain(|id, _| snapshot.iter().any(|j| j.id == *id));
            for j in snapshot {
                let title: SharedString = j.item.title.as_str().into();
                let row = match &j.state {
                    JobState::Queued => QueueRow { title, progress: 0.0, active: true, failed: false, leaving: false },
                    JobState::Running(p) => QueueRow { title, progress: p / 100.0, active: true, failed: false, leaving: false },
                    JobState::Failed(_) => QueueRow { title, progress: 0.0, active: false, failed: true, leaving: false },
                    JobState::Done(_) | JobState::Cancelled => {
                        let age = now.duration_since(*fin.entry(j.id).or_insert(now));
                        let hold = if matches!(j.state, JobState::Done(_)) { QUEUE_SAVED_HOLD } else { std::time::Duration::ZERO };
                        if age >= hold + QUEUE_LEAVE {
                            continue;
                        }
                        animating = true;
                        QueueRow { title, progress: 1.0, active: false, failed: false, leaving: age >= hold }
                    }
                };
                rows.push((j.id, row));
            }
        }
        sync_rows(&self.queue, &mut self.queue_ids.borrow_mut(), rows);
        if animating {
            // Look again soon, so the exit starts and the row is dropped on time.
            // Only the sidebar rows move; nothing about the files or results changed.
            self.queue_timer.start(slint::TimerMode::SingleShot, std::time::Duration::from_millis(120), || {
                GET.with(|g| {
                    if let Some(g) = g.borrow().as_ref() {
                        g.refresh_queue(&g.manager.snapshot());
                    }
                });
            });
        }
    }

    /// Brings the download list in line with the manager.
    fn refresh_jobs(&self) {
        let snapshot = self.manager.snapshot();
        let mut rows: Vec<(u64, JobRow)> = Vec::with_capacity(snapshot.len());
        let mut active = 0usize;
        for j in &snapshot {
            let (status, progress, failed) = match &j.state {
                JobState::Queued => ("Waiting".to_string(), 0.0, false),
                JobState::Running(p) => (format!("{:.0}%", p), p / 100.0, false),
                JobState::Done(_) => (SAVED.to_string(), 1.0, false),
                JobState::Failed(m) => (m.clone(), 0.0, true),
                JobState::Cancelled => ("Cancelled".to_string(), 0.0, false),
            };
            if j.state.is_active() {
                active += 1;
            }
            rows.push((
                j.id,
                JobRow { title: j.item.title.as_str().into(), status: status.into(), progress, active: j.state.is_active(), failed },
            ));
        }
        // Rows that stay keep their element; only the ones that changed are written.
        sync_rows(&self.jobs, &mut self.job_ids.borrow_mut(), rows);
        self.refresh_queue(&snapshot);
        if let Some(w) = self.win.upgrade() {
            let label = if active > 0 { format!("Downloads ({active})") } else { "Downloads".to_string() };
            w.set_downloads_label(label.into());
        }

        self.update_result_states_from(&snapshot);

        // Files that just arrived go into the library at once, with no folder rescan.
        let mut added_any = false;
        let mut importing: Vec<std::path::PathBuf> = Vec::new();
        if let Some(library) = &self.library {
            for j in &snapshot {
                if let JobState::Done(path) = &j.state {
                    if self.added.borrow_mut().insert(j.id) {
                        if j.stream && j.ready.is_some() {
                            self.finish_stream(j.item.clone(), path.clone());
                        }
                        if library.find(&path.to_string_lossy()).is_some() {
                            added_any = true; // listed already (it streamed in)
                        } else {
                            importing.push(path.clone());
                        }
                    }
                }
            }
        }

        // Reading the tags of a new file is disk work: do it off the UI thread, then continue.
        if let (false, Some(library)) = (importing.is_empty(), &self.library) {
            self.rescanning.set(true); // the song a click waits for is on its way into the library
            let library = library.clone();
            self.rt.spawn_blocking(move || {
                let all_read = importing.iter().all(|p| library.add_file(p).is_some());
                let _ = slint::invoke_from_event_loop(move || {
                    GET.with(|g| {
                        if let Some(g) = g.borrow().as_ref() {
                            g.update_result_states();
                            if all_read {
                                BROWSER.with(|b| {
                                    if let Some(b) = b.borrow().as_ref() {
                                        b.library_changed();
                                    }
                                });
                                g.library_ready();
                            } else {
                                // A file the tag reader could not use: read the folder again.
                                BROWSER.with(|b| {
                                    if let Some(b) = b.borrow().as_ref() {
                                        b.reload_library();
                                    }
                                });
                            }
                        }
                    });
                });
            });
        }

        // A streamed song was listed when its start arrived. If its job failed or was cancelled
        // the file is gone: take the entry out, so it is not shown as "In library".
        {
            let gone: Vec<(u64, std::path::PathBuf)> = self
                .provisional
                .borrow()
                .iter()
                .filter(|(id, path)| match snapshot.iter().find(|j| j.id == **id) {
                    Some(j) => matches!(j.state, JobState::Failed(_) | JobState::Cancelled),
                    None => !path.exists(),
                })
                .map(|(id, path)| (*id, path.clone()))
                .collect();
            for (id, path) in gone {
                self.provisional.borrow_mut().remove(&id);
                if let Some(library) = &self.library {
                    if library.remove_file(&path.to_string_lossy()) {
                        added_any = true;
                    }
                }
            }
            // A finished job owns its entry from here on.
            self.provisional.borrow_mut().retain(|id, _| snapshot.iter().any(|j| j.id == *id && j.state.is_active()));
        }

        // The song the user clicked: show its progress, and play it the moment it is saved.
        let awaited = self.awaiting.borrow().clone();
        if let (Some(id), Some(w)) = (awaited, self.win.upgrade()) {
            if let Some(j) = snapshot.iter().rev().find(|j| j.item.id == id) {
                // Streaming: the start of the file is saved, so play it while the rest arrives.
                if let (Some(path), true, Some(library)) = (&j.ready, j.state.is_active(), &self.library) {
                    let secs = j.item.duration_secs.unwrap_or(0);
                    let track = library.add_streaming(path, &j.item.title, &j.item.artist, secs.saturating_mul(1000));
                    self.provisional.borrow_mut().insert(j.id, path.clone());
                    *self.awaiting.borrow_mut() = None;
                    w.set_status(SharedString::default());
                    (self.send)(self.pick(&track.uri));
                    added_any = true;
                } else {
                match &j.state {
                    JobState::Queued => w.set_status("Waiting to download\u{2026}".into()),
                    JobState::Running(p) => {
                        w.set_status(format!("Downloading \u{201c}{}\u{201d}\u{2026} {p:.0}%", j.item.title).into())
                    }
                    JobState::Done(_) => {
                        match self.library.as_ref().and_then(|l| l.find_by_video_id(&id)) {
                            Some(track) => {
                                *self.awaiting.borrow_mut() = None;
                                w.set_status(SharedString::default());
                                (self.send)(self.pick(&track.uri));
                            }
                            // The folder is being read again: `library_ready` plays the song.
                            None if self.rescanning.get() => {
                                self.rescanning.set(true);
                                w.set_status("Opening\u{2026}".into())
                            }
                            None => {
                                *self.awaiting.borrow_mut() = None;
                                w.set_status("Downloaded, but the file could not be read as music.".into())
                            }
                        }
                    }
                    JobState::Failed(m) => {
                        w.set_status(m.as_str().into());
                        *self.awaiting.borrow_mut() = None;
                    }
                    JobState::Cancelled => {
                        w.set_status(SharedString::default());
                        *self.awaiting.borrow_mut() = None;
                    }
                }
                }
            }
        }

        // Jobs the manager no longer lists cannot come back, so their marks can go.
        let listed: std::collections::HashSet<u64> = snapshot.iter().map(|j| j.id).collect();
        self.added.borrow_mut().retain(|id| listed.contains(id));

        if added_any {
            self.update_result_states_from(&snapshot);
            BROWSER.with(|b| {
                if let Some(b) = b.borrow().as_ref() {
                    b.library_changed();
                }
            });
        }
    }
}

/// The search text of a `meowtify://play?q=<text>` link, decoded. Siri Shortcuts open these.
pub(crate) fn play_link_query(link: &str) -> Option<String> {
    let rest = link.trim().strip_prefix("meowtify://play")?;
    let query = rest.strip_prefix('?')?;
    let value = query.split('&').find_map(|pair| pair.strip_prefix("q="))?;
    let mut bytes: Vec<u8> = Vec::with_capacity(value.len());
    let raw = value.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        match raw[i] {
            b'+' => bytes.push(b' '),
            b'%' if raw.get(i + 1..i + 3).is_some_and(|h| h.iter().all(u8::is_ascii_hexdigit)) => {
                let hex = std::str::from_utf8(&raw[i + 1..i + 3]).unwrap_or("00");
                bytes.push(u8::from_str_radix(hex, 16).unwrap_or(b'?'));
                i += 2;
            }
            b => bytes.push(b),
        }
        i += 1;
    }
    let text = String::from_utf8_lossy(&bytes).trim().to_string();
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod play_link_tests {
    use super::play_link_query;

    #[test]
    fn decodes_the_query() {
        assert_eq!(play_link_query("meowtify://play?q=Midnight%20Drive").as_deref(), Some("Midnight Drive"));
        assert_eq!(play_link_query("meowtify://play?q=a+b%26c").as_deref(), Some("a b&c"));
        assert_eq!(play_link_query("meowtify://play?x=1&q=%E5%A4%9C").as_deref(), Some("\u{591c}"));
    }

    #[test]
    fn rejects_other_links() {
        assert_eq!(play_link_query("meowtify://jam/abc"), None);
        assert_eq!(play_link_query("meowtify://play?q="), None);
        assert_eq!(play_link_query("meowtify://play"), None);
    }
}
