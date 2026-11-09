//! The running part of autoplay. One task listens to the player's events; when the state
//! says a search is due, a second task does the network work and ends with a [`FillDone`].
//!
//! Songs are fetched in parallel batches ([`PARALLEL`] at a time) with the direct stream
//! download, which takes about a second per song, so the queue refills faster than songs
//! are skipped. A song YouTube cannot supply is looked for on the fallback providers
//! (SoundCloud, Audius).

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

use meowtify_core::{AppHandle, Command, Event, Place};
use meowtify_library::{config_dir, is_audio, Library, AHEAD_RANGE, BEHIND_RANGE};
use meowtify_provider::{
    main_artist, pick_match, resolve, song_key, valid_lastfm_key, Candidate, DeezerRadio, Item, LastFmSimilar, Provider, Recommender,
    YtMusicRadio,
};
use tokio::sync::{broadcast, mpsc, watch, Notify};
use tokio::task::JoinHandle;

use crate::rank::{rank, Context, Ranked, MAX_SECS, MIN_SECS};
use crate::state::{Added, FillDone, FillJob, State, ORPHAN_AGE};
use crate::taste::{Features, Taste};

/// How long one recommender may take.
const RECOMMEND_TIMEOUT: Duration = Duration::from_secs(9);
/// How long finding the playable version of one suggestion may take.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long one download may take.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(240);
/// How long fixing the container and tags of a fresh file may take.
const FINISH_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a fallback provider may take to search.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(20);
/// Suggestions that fail to download before a search gives up for now.
const MAX_FAILURES: usize = 8;
/// Songs fetched at the same time. Each one is a yt-dlp process of its own, which is the cost
/// to watch: four is fast and still light.
const WINDOW: usize = 4;
/// How many suggestions get their stream link looked up while the other lists are still coming.
const WARM: usize = 2;
/// The longest a search for suggestions waits for the slowest service. A slow answer only
/// costs this much when no service has answered yet; otherwise [`GATHER_GRACE`] applies.
const GATHER_CAP: Duration = Duration::from_secs(8);
/// The same limit for the second try, which runs when the first found nothing.
const GATHER_RETRY_CAP: Duration = Duration::from_secs(12);
/// The pause before that second try, so a brief network or service hiccup can pass.
const GATHER_RETRY_DELAY: Duration = Duration::from_millis(1500);
/// How long the other services get after the first one answered.
const GATHER_GRACE: Duration = Duration::from_millis(1500);

/// Learned taste is written to disk at most this often while autoplay runs.
const TASTE_SAVE_EVERY: Duration = Duration::from_secs(20);

/// Where what autoplay learned is saved.
pub fn taste_file() -> PathBuf {
    config_dir().join("taste.json")
}

/// Where autoplay saves the songs it fetches. They are not part of the user's library.
pub fn cache_dir() -> PathBuf {
    config_dir().join("cache").join("autoplay")
}

/// A spawned task that is aborted when this value is dropped. Children of a search live in
/// these, so ending the search (or the app) ends them too instead of detaching them.
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T: Send + 'static> AbortOnDrop<T> {
    fn spawn(f: impl std::future::Future<Output = T> + Send + 'static) -> Self {
        AbortOnDrop(tokio::spawn(f))
    }
}

impl<T> AbortOnDrop<T> {
    /// The result, or `None` when the task panicked or was cancelled.
    async fn join(&mut self) -> Option<T> {
        (&mut self.0).await.ok()
    }

    /// Aborts and waits until the task is gone.
    async fn cancel(mut self) {
        self.0.abort();
        let _ = (&mut self.0).await;
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The search that runs now.
struct Running {
    id: u64,
    task: AbortOnDrop<()>,
}

/// Resolves once the plan the search started under is no longer the current one.
async fn wait_stale(rx: &mut watch::Receiver<u64>, generation: u64) {
    while *rx.borrow() == generation {
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

fn publish_generation(tx: &watch::Sender<u64>, generation: u64) {
    tx.send_if_modified(|v| {
        let changed = *v != generation;
        *v = generation;
        changed
    });
}

struct Shared {
    enabled: AtomicBool,
    ahead: AtomicUsize,
    behind: AtomicUsize,
    lastfm_key: RwLock<String>,
    /// A search for songs is running.
    busy: AtomicBool,
    /// Where the Keep and Dislike buttons send their answer, once autoplay runs.
    feedback: Mutex<Option<tokio::sync::mpsc::UnboundedSender<(String, bool)>>>,
}

/// The settings of autoplay, changeable while it runs. Cheap to clone.
#[derive(Clone)]
pub struct Control(Arc<Shared>);

impl Control {
    pub fn new(enabled: bool, ahead: u8, behind: u8, lastfm_key: &str) -> Control {
        let c = Control(Arc::new(Shared {
            enabled: AtomicBool::new(enabled),
            ahead: AtomicUsize::new(0),
            behind: AtomicUsize::new(0),
            lastfm_key: RwLock::new(String::new()),
            busy: AtomicBool::new(false),
            feedback: Mutex::new(None),
        }));
        c.set_ahead(ahead);
        c.set_behind(behind);
        c.set_lastfm_key(lastfm_key);
        c
    }

    pub fn set_enabled(&self, on: bool) {
        self.0.enabled.store(on, Ordering::Relaxed);
    }

    /// Songs kept downloaded ahead of the playing one (limited to `AHEAD_RANGE`).
    pub fn set_ahead(&self, n: u8) {
        self.0.ahead.store(n.clamp(*AHEAD_RANGE.start(), *AHEAD_RANGE.end()) as usize, Ordering::Relaxed);
    }

    /// Played songs kept for Previous (limited to `BEHIND_RANGE`).
    pub fn set_behind(&self, n: u8) {
        self.0.behind.store(n.clamp(*BEHIND_RANGE.start(), *BEHIND_RANGE.end()) as usize, Ordering::Relaxed);
    }

    /// An empty or invalid key switches Last.fm off.
    pub fn set_lastfm_key(&self, key: &str) {
        *self.0.lastfm_key.write().unwrap() = key.trim().to_string();
    }

    /// True while autoplay is looking for songs, so the queue panel can say so.
    pub fn busy(&self) -> bool {
        self.0.busy.load(Ordering::Relaxed)
    }

    fn set_busy(&self, on: bool) {
        self.0.busy.store(on, Ordering::Relaxed);
    }

    /// The listener rated the song with this path: `true` for Keep, `false` for Dislike.
    /// It teaches autoplay; a dislike also replaces the songs that wait.
    pub fn rate(&self, uri: &str, liked: bool) {
        if let Some(tx) = self.0.feedback.lock().unwrap().as_ref() {
            let _ = tx.send((uri.to_string(), liked));
        }
    }

    fn enabled(&self) -> bool {
        self.0.enabled.load(Ordering::Relaxed)
    }

    fn ahead(&self) -> usize {
        self.0.ahead.load(Ordering::Relaxed)
    }

    fn behind(&self) -> usize {
        self.0.behind.load(Ordering::Relaxed)
    }

    fn lastfm_key(&self) -> String {
        self.0.lastfm_key.read().unwrap().clone()
    }
}

struct Env {
    handle: AppHandle,
    library: Arc<Library>,
    /// Finds and saves the songs (YouTube).
    provider: Arc<dyn Provider>,
    /// Tried in order for a song the main provider cannot supply.
    fallbacks: Vec<Arc<dyn Provider>>,
    control: Control,
    cache: PathBuf,
}

/// The stop signal of the autoplay task.
pub struct Autoplay {
    control: Control,
    stop: Arc<Notify>,
    rt: tokio::runtime::Handle,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl Autoplay {
    /// Starts listening. The cache folder is emptied first: songs that were not kept do not
    /// outlive the session.
    pub fn start(
        rt: &tokio::runtime::Handle,
        handle: AppHandle,
        library: Arc<Library>,
        provider: Arc<dyn Provider>,
        fallbacks: Vec<Arc<dyn Provider>>,
        control: Control,
    ) -> Autoplay {
        let cache = cache_dir();
        let _ = std::fs::remove_dir_all(&cache);
        let stop = Arc::new(Notify::new());
        let (fb_tx, fb_rx) = tokio::sync::mpsc::unbounded_channel();
        *control.0.feedback.lock().unwrap() = Some(fb_tx);
        let env = Arc::new(Env { handle, library, provider, fallbacks, control: control.clone(), cache });
        let task = rt.spawn(run(env, stop.clone(), fb_rx));
        Autoplay { control, stop, rt: rt.clone(), task: Mutex::new(Some(task)) }
    }

    pub fn control(&self) -> Control {
        self.control.clone()
    }

    /// Ends the task and waits (a few seconds at most) until it and everything it started
    /// is gone. They hold app handles, which the app needs released to shut down. Call it
    /// from a thread that is not part of the runtime.
    pub fn stop(&self) {
        self.stop.notify_one();
        let Some(task) = self.task.lock().unwrap().take() else { return };
        let _ = self.rt.block_on(async { tokio::time::timeout(Duration::from_secs(5), task).await });
    }
}

async fn run(env: Arc<Env>, stop: Arc<Notify>, mut ratings: tokio::sync::mpsc::UnboundedReceiver<(String, bool)>) {
    let mut events = env.handle.subscribe();
    // A finished search reports (its id, the plan it started under, its result). `None` is a
    // search that panicked.
    let (done_tx, mut done_rx) = mpsc::channel::<(u64, u64, Option<FillDone>)>(1);
    let taste = tokio::task::spawn_blocking(|| Taste::load(&taste_file())).await.unwrap_or_default();
    let mut state = State::new(env.control.ahead(), env.control.behind(), taste);
    let (gen_tx, gen_rx) = watch::channel(state.generation());
    let mut task: Option<Running> = None;
    let mut next_id = 0u64;
    let mut last_taste_save = Instant::now();
    loop {
        tokio::select! {
            _ = stop.notified() => break,
            Some((id, generation, result)) = done_rx.recv() => {
                if task.as_ref().is_some_and(|t| t.id == id) {
                    task = None;
                }
                let now = Instant::now();
                match result {
                    Some(d) if generation == state.generation() => {
                        if state.done(d, now) {
                            // The last song ended before the next one was ready: start it.
                            let _ = env.handle.send(Command::Next).await;
                        }
                    }
                    Some(d) => state.done_stale(d),
                    None => state.failed(now),
                }
            }
            rated = ratings.recv() => {
                if let Some((uri, liked)) = rated {
                    state.rate(&uri, liked);
                }
            }
            ev = events.recv() => match ev {
                Ok(Event::PlaybackChanged(s)) => {
                    state.playback(s.as_ref());
                    publish_generation(&gen_tx, state.generation());
                }
                Ok(Event::QueueChanged(q)) => {
                    let now = Instant::now();
                    state.configure(env.control.ahead(), env.control.behind());
                    if !env.control.enabled() && task.is_some() {
                        // Switched off while a search runs: it must stop queueing songs.
                        state.supersede();
                    }
                    publish_generation(&gen_tx, state.generation());
                    let job = state.queue(&q, env.control.enabled(), task.is_some(), now);
                    if let Some(job) = job {
                        next_id += 1;
                        let (id, generation) = (next_id, state.generation());
                        let (env, tx, rx) = (env.clone(), done_tx.clone(), gen_rx.clone());
                        // The search runs as a child of this wrapper, so aborting the wrapper
                        // aborts the search, and a panic in the search is reported, not lost.
                        let wrapper = AbortOnDrop::spawn(async move {
                            let mut search = AbortOnDrop::spawn(fill(env, job, generation, rx));
                            let result = search.join().await;
                            let _ = tx.send((id, generation, result)).await;
                        });
                        task = Some(Running { id, task: wrapper });
                    }
                    if let Some(protect) = state.sweep_due(now) {
                        let cache = env.cache.clone();
                        let removed = tokio::task::spawn_blocking(move || sweep(&cache, &protect)).await.unwrap_or_default();
                        for uri in &removed {
                            env.library.forget_transient(uri);
                        }
                        state.forgotten(&removed);
                    }
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            },
        }

        // Serializing the whole graph on every small change is wasted work and memory: save at
        // most every TASTE_SAVE_EVERY. What is still unsaved is written when autoplay stops.
        if last_taste_save.elapsed() >= TASTE_SAVE_EVERY {
            if let Some(json) = state.taste_json() {
                last_taste_save = Instant::now();
                let _ = tokio::task::spawn_blocking(move || save_taste(&json)).await;
            }
        }
        env.control.set_busy(task.is_some());
    }
    // Stop the search and everything it started, then wait until nothing but this task still
    // holds the environment (and with it the app handle).
    if let Some(t) = task.take() {
        t.task.cancel().await;
    }
    for _ in 0..300 {
        if Arc::strong_count(&env) <= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if let Some(json) = state.taste_json() {
        let _ = tokio::task::spawn_blocking(move || save_taste(&json)).await;
    }
    env.control.set_busy(false);
}

/// Writes the taste file through a temporary file, so a crash cannot leave half of it.
fn save_taste(json: &str) {
    let path = taste_file();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Finds songs for `job`, makes them playable and queues them. Up to [`WINDOW`] songs are
/// fetched at once, and each is queued the moment it and the ones before it are ready, so the
/// first song of a new queue is there after one fetch, not after the whole batch.
async fn fill(env: Arc<Env>, mut job: FillJob, generation: u64, mut plan: watch::Receiver<u64>) -> FillDone {
    let cancelled = |plan: &watch::Receiver<u64>| *plan.borrow() != generation || !env.control.enabled();
    let nothing = |job: &FillJob| FillDone { seed_key: job.seed_key.clone(), pool: Vec::new(), added: Vec::new() };
    // The warm-up lookups stay alive until this search ends.
    let (mut pool, _warm): (Vec<Ranked>, Vec<AbortOnDrop<()>>) = match job.pool.take() {
        Some(p) if !p.is_empty() => (p, Vec::new()),
        _ => tokio::select! {
            g = gather_retry(&env, &job) => g,
            _ = wait_stale(&mut plan, generation) => return nothing(&job),
        },
    };
    let mut exclude = job.exclude.clone();
    let mut added: Vec<Added> = Vec::new();
    let mut failures = 0;
    type Fetch = (String, Features, AbortOnDrop<Option<(String, Item)>>);
    // Dropping this queue (on any exit, including an abort) aborts every fetch in it.
    let mut inflight: VecDeque<Fetch> = VecDeque::new();
    loop {
        if cancelled(&plan) {
            break;
        }
        // Keep the window full, but never start more than the queue still needs.
        while inflight.len() < WINDOW && added.len() + inflight.len() < job.need && !pool.is_empty() {
            let ranked = pool.remove(0);
            let key = song_key(&ranked.cand.title, &ranked.cand.artist);
            if exclude.contains(&key) || inflight.iter().any(|(k, _, _)| *k == key) {
                continue;
            }
            let (env, cand) = (env.clone(), ranked.cand);
            inflight.push_back((key, ranked.feat, AbortOnDrop::spawn(async move { prepare(&env, &cand).await })));
        }
        // Queue in suggestion order, so the closest song plays first.
        let Some((key, feat, mut handle)) = inflight.pop_front() else { break };
        let result = tokio::select! {
            r = handle.join() => r.flatten(),
            _ = wait_stale(&mut plan, generation) => break,
        };
        match result {
            Some((uri, item)) => {
                // The plan may have changed while the song downloaded.
                if cancelled(&plan) {
                    break;
                }
                if env.handle.send(Command::Enqueue { uri: uri.as_str().into(), place: Place::Auto }).await.is_err() {
                    break;
                }
                exclude.insert(key.clone());
                added.push(Added { key, artist: main_artist(&item.artist), uri, feat });
            }
            None => failures += 1,
        }
        if failures >= MAX_FAILURES {
            break;
        }
    }
    FillDone { seed_key: job.seed_key, pool, added }
}

/// Asks every recommender at once and merges the answers with the songs this listener played
/// after the seed. It does not wait for a slow service: once one answers, the others get
/// [`GATHER_GRACE`] more, and the whole search is capped at [`GATHER_CAP`].
async fn gather_retry(env: &Env, job: &FillJob) -> (Vec<Ranked>, Vec<AbortOnDrop<()>>) {
    let first = gather(env, job, GATHER_CAP).await;
    if !first.0.is_empty() {
        return first;
    }
    eprintln!("autoplay: no suggestions for {:?}, trying once more", job.seed_key);
    tokio::time::sleep(GATHER_RETRY_DELAY).await;
    let second = gather(env, job, GATHER_RETRY_CAP).await;
    if second.0.is_empty() {
        eprintln!("autoplay: still no suggestions for {:?}", job.seed_key);
    }
    second
}

async fn gather(env: &Env, job: &FillJob, cap: Duration) -> (Vec<Ranked>, Vec<AbortOnDrop<()>>) {
    let mut recommenders: Vec<Arc<dyn Recommender>> = vec![Arc::new(YtMusicRadio), Arc::new(DeezerRadio)];
    let key = env.control.lastfm_key();
    if valid_lastfm_key(&key) {
        recommenders.push(Arc::new(LastFmSimilar { key }));
    }
    let total = recommenders.len();
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<Candidate>>();
    // Services that have not answered when the search is done are cancelled, not left to run.
    let mut asking: Vec<AbortOnDrop<()>> = Vec::new();
    let mut warm: Vec<AbortOnDrop<()>> = Vec::new();
    for r in recommenders {
        let (seed, tx) = (job.seed.clone(), tx.clone());
        asking.push(AbortOnDrop::spawn(async move {
            let list = match tokio::time::timeout(RECOMMEND_TIMEOUT, r.similar(&seed)).await {
                Ok(Ok(list)) => list,
                Ok(Err(e)) => {
                    eprintln!("autoplay: {} failed: {e}", r.name());
                    Vec::new()
                }
                Err(_) => {
                    eprintln!("autoplay: {} timed out", r.name());
                    Vec::new()
                }
            };
            let _ = tx.send(list);
        }));
    }
    drop(tx);
    let mut lists: Vec<Vec<Candidate>> = Vec::new();
    if !job.history.is_empty() {
        lists.push(job.history.clone());
    }
    let hard = tokio::time::Instant::now() + cap;
    let mut deadline = hard;
    let mut answered = 0;
    let mut warmed = false;
    while answered < total {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(list)) => {
                answered += 1;
                if !list.is_empty() {
                    deadline = deadline.min(tokio::time::Instant::now() + GATHER_GRACE);
                    // The lookup of a song's stream link is the slowest step (about 2 s). Start
                    // it for the first suggestions now, while the other lists are still coming.
                    // The fill joins the same lookup, so none of it is done twice.
                    if !warmed {
                        let mut n = 0;
                        for c in &list {
                            let Some(item) = c.item.clone() else { continue };
                            if job.exclude.contains(&song_key(&c.title, &c.artist)) {
                                continue;
                            }
                            let provider = env.provider.clone();
                            warm.push(AbortOnDrop::spawn(async move { provider.prefetch(&item).await }));
                            n += 1;
                            if n == WARM {
                                break;
                            }
                        }
                        warmed = n > 0;
                    }
                    lists.push(list);
                }
            }
            _ => break,
        }
    }
    let cx = Context {
        seed: &job.seed,
        exclude: &job.exclude,
        recent_artists: &job.recent_artists,
        disliked: &job.disliked,
        taste: &job.taste,
    };
    drop(asking);
    (rank(&lists, &cx), warm)
}

/// Makes a suggestion playable: the file already in the library or the cache, or a new
/// download from YouTube, or from a fallback provider when YouTube has no copy.
/// Returns the path to queue and the item it came from.
async fn prepare(env: &Env, cand: &Candidate) -> Option<(String, Item)> {
    if let Some(found) = from_main(env, cand).await {
        return Some(found);
    }
    for provider in &env.fallbacks {
        if let Some(found) = from_fallback(env, provider, cand).await {
            return Some(found);
        }
    }
    None
}

async fn from_main(env: &Env, cand: &Candidate) -> Option<(String, Item)> {
    let item = tokio::time::timeout(RESOLVE_TIMEOUT, resolve(cand)).await.ok()??;
    if item.duration_secs.is_some_and(|s| !(MIN_SECS..=MAX_SECS).contains(&s)) {
        return None;
    }
    if let Some(t) = env.library.find_by_video_id(&item.id) {
        return Some((t.uri.to_string(), item));
    }
    let path = match cached(&env.cache, &item.id) {
        Some(p) => {
            touch(&p);
            p
        }
        None => {
            std::fs::create_dir_all(&env.cache).ok()?;
            let progress: meowtify_provider::ProgressFn = Arc::new(|_| {});
            let ready: meowtify_provider::ReadyFn = Arc::new(|_| {});
            // The direct download is the fast path (about a second); it falls back to yt-dlp by itself.
            let path = tokio::time::timeout(DOWNLOAD_TIMEOUT, env.provider.stream(&item, &env.cache, progress, ready)).await.ok()?.ok()?;
            // Makes the file seekable and tagged; a failure leaves a playable file.
            let _ = tokio::time::timeout(FINISH_TIMEOUT, env.provider.finish(&item, &path)).await;
            path
        }
    };
    Some((register(env, &path, &item, cand), item))
}

async fn from_fallback(env: &Env, provider: &Arc<dyn Provider>, cand: &Candidate) -> Option<(String, Item)> {
    let query = format!("{} {}", cand.artist, cand.title);
    let found = tokio::time::timeout(SEARCH_TIMEOUT, provider.search(query.trim(), 8)).await.ok()?.ok()?;
    let item = pick_match(&found, &cand.title, &cand.artist, cand.duration_secs)?;
    if item.duration_secs.is_some_and(|s| !(MIN_SECS..=MAX_SECS).contains(&s)) {
        return None;
    }
    let path = match cached(&env.cache, &item.id) {
        Some(p) => {
            touch(&p);
            p
        }
        None => {
            std::fs::create_dir_all(&env.cache).ok()?;
            let progress: meowtify_provider::ProgressFn = Arc::new(|_| {});
            tokio::time::timeout(DOWNLOAD_TIMEOUT, provider.download(&item, &env.cache, progress)).await.ok()?.ok()?
        }
    };
    Some((register(env, &path, &item, cand), item))
}

/// Makes the file playable by the player without listing it in the library.
fn register(env: &Env, path: &Path, item: &Item, cand: &Candidate) -> String {
    let secs = item.duration_secs.or(cand.duration_secs).unwrap_or(0);
    env.library.add_transient(path, &item.title, &item.artist, secs.saturating_mul(1000)).uri.to_string()
}

/// Marks a reused cache file as new. The sweep deletes unprotected files older than
/// [`ORPHAN_AGE`], and a file that is queued before the sweep's list of protected paths knows it
/// (the queue event has not arrived yet) must not look old, or it is deleted while queued.
fn touch(path: &Path) {
    if let Ok(f) = std::fs::File::options().write(true).open(path) {
        let _ = f.set_modified(SystemTime::now());
    }
}

/// A finished file of the cache for a video id.
fn cached(dir: &Path, id: &str) -> Option<PathBuf> {
    let needle = format!("[{id}].");
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| is_audio(p) && p.file_name().is_some_and(|n| n.to_string_lossy().contains(&needle)))
}

/// Deletes every cache file that is not in `protect` and is older than [`ORPHAN_AGE`].
/// Returns the paths it deleted. A file the player still holds stays and is tried again later.
fn sweep(dir: &Path, protect: &HashSet<String>) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let now = SystemTime::now();
    let mut removed = Vec::new();
    for e in rd.flatten() {
        let Ok(meta) = e.metadata() else { continue };
        let path = e.path();
        let key = path.to_string_lossy().into_owned();
        let old = meta.modified().ok().and_then(|m| now.duration_since(m).ok()).is_some_and(|age| age >= ORPHAN_AGE);
        if meta.is_file() && old && !protect.contains(&key) && std::fs::remove_file(&path).is_ok() {
            removed.push(key);
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, len: usize, age_secs: u64) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, vec![0u8; len]).unwrap();
        let t = SystemTime::now() - Duration::from_secs(age_secs);
        std::fs::File::options().write(true).open(&p).unwrap().set_modified(t).unwrap();
        p
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("meowtify-autoplay-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn sweep_removes_old_unprotected_files_only() {
        let d = temp("sweep");
        let orphan = write(&d, "orphan [aaaaaaaaaaa].m4a", 100, 7200);
        let playing = write(&d, "playing [ccccccccccc].m4a", 100, 8000);
        let fresh = write(&d, "fresh [ddddddddddd].m4a", 100, 5);
        let protect: HashSet<String> = [playing.to_string_lossy().into_owned()].into();
        let removed = sweep(&d, &protect);
        assert_eq!(removed, vec![orphan.to_string_lossy().into_owned()]);
        assert!(!orphan.exists() && playing.exists() && fresh.exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_reused_file_that_is_touched_survives_the_sweep() {
        let d = temp("touch");
        let reused = write(&d, "reused [eeeeeeeeeee].m4a", 100, 7200);
        touch(&reused);
        let removed = sweep(&d, &HashSet::new());
        assert!(removed.is_empty() && reused.exists(), "a file queued a moment ago looked old");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Needs the network: runs a whole fill for 8 songs and prints when each one reaches the
    /// queue. Run with `cargo test -p meowtify-autoplay live_queue -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn live_queue_fill_speed() {
        let tools = meowtify_provider::Tools::locate_or_default(&config_dir().join("tools"));
        let provider: Arc<dyn Provider> = Arc::new(meowtify_provider::YouTube::new(tools));
        let cache = temp("fill");
        let (handle, mut commands) = AppHandle::new(64, 64);
        let env = Arc::new(Env {
            handle,
            library: Arc::new(Library::new(None)),
            provider,
            fallbacks: Vec::new(),
            control: Control::new(true, 8, 3, ""),
            cache: cache.clone(),
        });
        let seed = meowtify_provider::Seed { title: "Blinding Lights".into(), artist: "The Weeknd".into(), video_id: None, duration_secs: Some(200) };
        let job = FillJob {
            seed_key: song_key(&seed.title, &seed.artist),
            seed,
            need: 8,
            exclude: HashSet::new(),
            recent_artists: Vec::new(),
            disliked: HashSet::new(),
            pool: None,
            history: Vec::new(),
            taste: Taste::new(),
        };
        let started = Instant::now();
        let reader = tokio::spawn(async move {
            let mut times = Vec::new();
            while let Some(c) = commands.recv().await {
                if matches!(c, Command::Enqueue { .. }) {
                    times.push(started.elapsed());
                }
            }
            times
        });
        let (_plan_tx, plan_rx) = watch::channel(0u64);
        let done = fill(env.clone(), job, 0, plan_rx).await;
        println!("fill returned after {:?} with {} songs", started.elapsed(), done.added.len());
        drop(env);
        let times = reader.await.unwrap();
        for (i, t) in times.iter().enumerate() {
            println!("song {} queued after {:?}", i + 1, t);
        }
        let _ = std::fs::remove_dir_all(&cache);
        assert!(done.added.len() >= 6, "too few songs: {}", done.added.len());
    }

    #[test]
    fn settings_are_limited_to_their_range() {
        let c = Control::new(true, 200, 200, "  key  ");
        assert_eq!((c.ahead(), c.behind()), (30, 20));
        c.set_ahead(0);
        c.set_behind(0);
        assert_eq!((c.ahead(), c.behind()), (2, 0));
        assert_eq!(c.lastfm_key(), "key");
    }

    /// Needs the network: asks the services for songs close to a known one, then fetches the
    /// first three at once, the way a refill does, and prints the times.
    /// Run with `cargo test -p meowtify-autoplay live -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn live_suggestions_and_a_parallel_batch() {
        let seed = meowtify_provider::Seed {
            title: "Blinding Lights".into(),
            artist: "The Weeknd".into(),
            video_id: None,
            duration_secs: Some(200),
        };
        let recommenders: Vec<Arc<dyn Recommender>> = vec![Arc::new(YtMusicRadio), Arc::new(DeezerRadio)];
        let mut lists = Vec::new();
        for r in &recommenders {
            let started = Instant::now();
            let list = r.similar(&seed).await;
            println!("{}: {} in {:?}", r.name(), list.as_ref().map_or(0, |l| l.len()), started.elapsed());
            lists.push(list.unwrap_or_default());
        }
        let none = HashSet::new();
        let taste = Taste::new();
        let cx = Context { seed: &seed, exclude: &none, recent_artists: &[], disliked: &none, taste: &taste };
        let ranked: Vec<_> = rank(&lists, &cx).into_iter().map(|r| r.cand).collect();
        for c in ranked.iter().take(8) {
            println!("{:>14}  {} - {}  ({:?}s)", c.source, c.artist, c.title, c.duration_secs);
        }
        assert!(ranked.len() >= 5, "too few suggestions");

        let tools = meowtify_provider::Tools::locate_or_default(&config_dir().join("tools"));
        let youtube: Arc<dyn Provider> = Arc::new(meowtify_provider::YouTube::new(tools));
        let dir = temp("live");
        let started = Instant::now();
        let mut tasks = Vec::new();
        for cand in ranked.iter().take(3).cloned() {
            let (youtube, dir) = (youtube.clone(), dir.clone());
            tasks.push(tokio::spawn(async move {
                let t = Instant::now();
                let item = resolve(&cand).await?;
                let path = youtube.stream(&item, &dir, Arc::new(|_| {}), Arc::new(|_| {})).await.ok()?;
                let _ = youtube.finish(&item, &path).await;
                Some((item.title, t.elapsed(), std::fs::metadata(&path).ok()?.len()))
            }));
        }
        let mut ok = 0;
        for t in tasks {
            if let Some((title, took, len)) = t.await.unwrap() {
                println!("fetched {title:?}: {len} bytes in {took:?}");
                ok += 1;
            }
        }
        println!("batch of 3 done in {:?}", started.elapsed());
        let _ = std::fs::remove_dir_all(&dir);
        assert!(ok >= 2, "at most one fetch may fail");
    }
}
