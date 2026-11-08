//! Download queue. Runs up to [`DEFAULT_PARALLEL`] downloads at once and keeps
//! the state of every job so the UI can show a list with progress.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio::task::AbortHandle;

use crate::{Item, ProgressFn, Provider, ReadyFn};

/// Finished, failed and cancelled jobs kept in the list. Older ones are dropped when new jobs
/// come, so a long session does not grow the list (and the work to show it) without limit.
const MAX_HISTORY: usize = 100;

/// Downloads that run at the same time. Each is one yt-dlp process, so keep it small.
pub const DEFAULT_PARALLEL: usize = 3;

#[derive(Debug, Clone, PartialEq)]
pub enum JobState {
    Queued,
    /// Percentage from 0.0 to 100.0.
    Running(f32),
    Done(PathBuf),
    Failed(String),
    Cancelled,
}

/// Drops the oldest finished jobs beyond [`MAX_HISTORY`].
fn trim_history(jobs: &mut Vec<Job>) {
    let mut over = jobs.iter().filter(|j| !j.state.is_active()).count().saturating_sub(MAX_HISTORY);
    if over > 0 {
        jobs.retain(|j| {
            if over > 0 && !j.state.is_active() {
                over -= 1;
                false
            } else {
                true
            }
        });
    }
}

impl JobState {
    /// Queued or running.
    pub fn is_active(&self) -> bool {
        matches!(self, JobState::Queued | JobState::Running(_))
    }
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: u64,
    pub item: Item,
    pub dest: PathBuf,
    pub state: JobState,
    /// Play while downloading: the provider may report the file early through `ready`.
    pub stream: bool,
    /// Set once the start of the file can be played. The job is still running then.
    pub ready: Option<PathBuf>,
}

struct Inner {
    provider: Arc<dyn Provider>,
    jobs: Mutex<Vec<Job>>,
    next_id: AtomicU64,
    wake: Notify,
    /// Handles of the running jobs, by job id.
    running: Mutex<HashMap<u64, AbortHandle>>,
    slots: Arc<Semaphore>,
    on_change: Box<dyn Fn() + Send + Sync>,
    /// Set when the last manager handle is gone: the worker ends.
    closed: AtomicBool,
    closing: Notify,
}

/// Ends the queue when the last [`DownloadManager`] clone is dropped. The worker and the
/// running jobs hold `Inner` themselves, so `Inner` cannot tell.
struct StopOnDrop(Arc<Inner>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.closed.store(true, Ordering::Release);
        self.0.closing.notify_one();
        for handle in self.0.running.lock().unwrap().values() {
            handle.abort();
        }
    }
}

/// Cheap to clone. All clones share one queue.
#[derive(Clone)]
pub struct DownloadManager {
    inner: Arc<Inner>,
    _stop: Arc<StopOnDrop>,
}

impl DownloadManager {
    /// Creates the queue and starts its worker on `rt`. `on_change` is called,
    /// from any thread, whenever a job is added or its state changes.
    pub fn start(
        provider: Arc<dyn Provider>,
        rt: &tokio::runtime::Handle,
        on_change: impl Fn() + Send + Sync + 'static,
    ) -> DownloadManager {
        Self::start_with(provider, rt, DEFAULT_PARALLEL, on_change)
    }

    /// Like [`start`](Self::start), with `parallel` (at least 1) downloads at once.
    pub fn start_with(
        provider: Arc<dyn Provider>,
        rt: &tokio::runtime::Handle,
        parallel: usize,
        on_change: impl Fn() + Send + Sync + 'static,
    ) -> DownloadManager {
        let inner = Arc::new(Inner {
            provider,
            jobs: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            wake: Notify::new(),
            running: Mutex::new(HashMap::new()),
            slots: Arc::new(Semaphore::new(parallel.max(1))),
            on_change: Box::new(on_change),
            closed: AtomicBool::new(false),
            closing: Notify::new(),
        });
        rt.spawn(worker(inner.clone()));
        DownloadManager { _stop: Arc::new(StopOnDrop(inner.clone())), inner }
    }

    pub fn provider(&self) -> &Arc<dyn Provider> {
        &self.inner.provider
    }

    /// Adds a job at the end of the queue. Returns `None` when the same item is
    /// already queued or running.
    pub fn enqueue(&self, item: Item, dest: PathBuf) -> Option<u64> {
        self.add(item, dest)
    }

    /// Like [`enqueue`](Self::enqueue), but the job runs before other waiting
    /// jobs. A job that is already running is not interrupted. If the item is
    /// already waiting, it is moved forward.
    pub fn enqueue_next(&self, item: Item, dest: PathBuf) -> u64 {
        self.enqueue_front(item, dest, false)
    }

    /// Like [`enqueue_next`](Self::enqueue_next), and the job reports [`Job::ready`] as soon
    /// as the start of the file can be played. If the item is already active, that job is
    /// kept as it is (it may not stream).
    pub fn enqueue_stream(&self, item: Item, dest: PathBuf) -> u64 {
        self.enqueue_front(item, dest, true)
    }

    fn enqueue_front(&self, item: Item, dest: PathBuf, stream: bool) -> u64 {
        // Look for a duplicate and insert in one critical section, so a job that finishes
        // or is cancelled in between cannot make the lookup fail.
        let (id, added) = {
            let mut jobs = self.inner.jobs.lock().unwrap();
            let dup = jobs.iter().position(|j| j.state.is_active() && j.item.provider == item.provider && j.item.id == item.id);
            match dup {
                Some(pos) => {
                    let id = jobs[pos].id;
                    if jobs[pos].state == JobState::Queued {
                        let job = jobs.remove(pos);
                        let at = jobs.iter().position(|j| j.state == JobState::Queued).unwrap_or(jobs.len());
                        jobs.insert(at, job);
                    }
                    (id, false)
                }
                None => {
                    let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
                    let job = Job { id, item, dest, state: JobState::Queued, stream, ready: None };
                    trim_history(&mut jobs);
                    let at = jobs.iter().position(|j| j.state == JobState::Queued).unwrap_or(jobs.len());
                    jobs.insert(at, job);
                    (id, true)
                }
            }
        };
        if added {
            (self.inner.on_change)();
            self.inner.wake.notify_one();
        }
        id
    }

    fn add(&self, item: Item, dest: PathBuf) -> Option<u64> {
        let id = {
            let mut jobs = self.inner.jobs.lock().unwrap();
            let dup = jobs.iter().any(|j| j.state.is_active() && j.item.provider == item.provider && j.item.id == item.id);
            if dup {
                return None;
            }
            let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
            let job = Job { id, item, dest, state: JobState::Queued, stream: false, ready: None };
            trim_history(&mut jobs);
            jobs.push(job);
            id
        };
        (self.inner.on_change)();
        self.inner.wake.notify_one();
        Some(id)
    }

    /// Stops a queued or running job. Finished jobs are left alone.
    pub fn cancel(&self, id: u64) {
        let changed = {
            let mut jobs = self.inner.jobs.lock().unwrap();
            match jobs.iter_mut().find(|j| j.id == id && j.state.is_active()) {
                Some(j) => {
                    j.state = JobState::Cancelled;
                    true
                }
                None => false,
            }
        };
        if changed {
            // Aborting drops the download future, which kills yt-dlp.
            if let Some(handle) = self.inner.running.lock().unwrap().get(&id) {
                handle.abort();
            }
            (self.inner.on_change)();
        }
    }

    /// Removes finished, failed and cancelled jobs from the list.
    pub fn clear_finished(&self) {
        self.inner.jobs.lock().unwrap().retain(|j| j.state.is_active());
        (self.inner.on_change)();
    }

    pub fn snapshot(&self) -> Vec<Job> {
        self.inner.jobs.lock().unwrap().clone()
    }

    /// True when no job is queued or running.
    pub fn is_idle(&self) -> bool {
        !self.inner.jobs.lock().unwrap().iter().any(|j| j.state.is_active())
    }
}

fn set_state(inner: &Inner, id: u64, state: JobState) {
    {
        let mut jobs = inner.jobs.lock().unwrap();
        // A job cancelled meanwhile keeps its state.
        if let Some(j) = jobs.iter_mut().find(|j| j.id == id && j.state != JobState::Cancelled) {
            j.state = state;
        }
    }
    (inner.on_change)();
}

fn next_queued(inner: &Inner) -> Option<Job> {
    let mut jobs = inner.jobs.lock().unwrap();
    let j = jobs.iter_mut().find(|j| j.state == JobState::Queued)?;
    j.state = JobState::Running(0.0);
    Some(j.clone())
}

async fn worker(inner: Arc<Inner>) {
    loop {
        // Wait for a free slot first, so a job is marked running only when it starts.
        let permit = tokio::select! {
            p = inner.slots.clone().acquire_owned() => match p {
                Ok(p) => p,
                Err(_) => return,
            },
            _ = inner.closing.notified() => return,
        };
        if inner.closed.load(Ordering::Acquire) {
            return;
        }
        let Some(job) = next_queued(&inner) else {
            drop(permit);
            tokio::select! {
                _ = inner.wake.notified() => {}
                _ = inner.closing.notified() => return,
            }
            continue;
        };
        // The Running notification is sent by `run_job`, once the job can be cancelled.
        tokio::spawn(run_job(inner.clone(), job, permit));
    }
}

async fn run_job(inner: Arc<Inner>, job: Job, _slot: OwnedSemaphorePermit) {
    // Report whole-percent steps only, so the UI is not flooded.
    let last = Arc::new(AtomicU64::new(u64::MAX));
    let progress: ProgressFn = {
        let inner = inner.clone();
        let id = job.id;
        Arc::new(move |p: f32| {
            let step = p as u64;
            if last.swap(step, Ordering::Relaxed) != step {
                let mut jobs = inner.jobs.lock().unwrap();
                if let Some(j) = jobs.iter_mut().find(|j| j.id == id && matches!(j.state, JobState::Running(_))) {
                    j.state = JobState::Running(p);
                }
                drop(jobs);
                (inner.on_change)();
            }
        })
    };

    // The provider call waits for `go`, so it cannot start before the job is registered
    // as cancellable and the Running notification went out.
    let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
    let task = {
        let provider = inner.provider.clone();
        let (item, dest) = (job.item.clone(), job.dest.clone());
        if job.stream {
            let ready: ReadyFn = {
                let inner = inner.clone();
                let id = job.id;
                Arc::new(move |path: &std::path::Path| {
                    {
                        let mut jobs = inner.jobs.lock().unwrap();
                        if let Some(j) = jobs.iter_mut().find(|j| j.id == id && j.state.is_active()) {
                            j.ready = Some(path.to_path_buf());
                        }
                    }
                    (inner.on_change)();
                })
            };
            tokio::spawn(async move {
                go_rx.await.map_err(|_| crate::ProviderError::Tool("download was cancelled before it started".into()))?;
                provider.stream(&item, &dest, progress, ready).await
            })
        } else {
            tokio::spawn(async move {
                go_rx.await.map_err(|_| crate::ProviderError::Tool("download was cancelled before it started".into()))?;
                provider.download(&item, &dest, progress).await
            })
        }
    };
    inner.running.lock().unwrap().insert(job.id, task.abort_handle());
    let cancelled = inner.jobs.lock().unwrap().iter().any(|j| j.id == job.id && j.state == JobState::Cancelled);
    (inner.on_change)();
    if cancelled {
        task.abort();
    } else {
        // A cancel that arrives from now on finds the handle and aborts the task.
        let _ = go_tx.send(());
    }
    let result = task.await;
    inner.running.lock().unwrap().remove(&job.id);

    match result {
        Ok(Ok(path)) => set_state(&inner, job.id, JobState::Done(path)),
        Ok(Err(e)) => set_state(&inner, job.id, JobState::Failed(e.to_string())),
        Err(join) if join.is_cancelled() => set_state(&inner, job.id, JobState::Cancelled),
        Err(join) => set_state(&inner, job.id, JobState::Failed(format!("download task failed: {join}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProviderError, Result};
    use async_trait::async_trait;
    use std::path::Path;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    /// Fake provider. Titles choose the behaviour: "fail" errors, "slow" waits.
    struct Fake {
        started: AtomicUsize,
    }

    #[async_trait]
    impl Provider for Fake {
        async fn search(&self, _q: &str, _l: u32) -> Result<Vec<Item>> {
            Ok(Vec::new())
        }
        async fn resolve(&self, _u: &str) -> Result<Vec<Item>> {
            Ok(Vec::new())
        }
        async fn download(&self, item: &Item, dest: &Path, progress: ProgressFn) -> Result<PathBuf> {
            self.started.fetch_add(1, Ordering::SeqCst);
            match item.title.as_str() {
                "fail" => Err(ProviderError::Tool("boom".into())),
                "slow" => {
                    progress(10.0);
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Ok(dest.join("never"))
                }
                _ => {
                    progress(50.0);
                    Ok(dest.join(format!("{}.m4a", item.id)))
                }
            }
        }
    }

    fn item(id: &str, title: &str) -> Item {
        Item { provider: "fake", id: id.into(), title: title.into(), artist: String::new(), duration_secs: None, url: String::new(), thumbnail: None }
    }

    fn manager() -> (DownloadManager, Arc<Fake>, Arc<AtomicUsize>) {
        let fake = Arc::new(Fake { started: AtomicUsize::new(0) });
        let changes = Arc::new(AtomicUsize::new(0));
        let c = changes.clone();
        // One slot keeps the order-based tests exact.
        let m = DownloadManager::start_with(fake.clone(), &tokio::runtime::Handle::current(), 1, move || {
            c.fetch_add(1, Ordering::SeqCst);
        });
        (m, fake, changes)
    }

    async fn wait_idle(m: &DownloadManager) {
        for _ in 0..200 {
            if m.is_idle() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("queue did not become idle");
    }

    #[tokio::test]
    async fn runs_jobs_in_order_and_reports_results() {
        let (m, _fake, changes) = manager();
        m.enqueue(item("a", "ok"), "d".into()).unwrap();
        m.enqueue(item("b", "fail"), "d".into()).unwrap();
        m.enqueue(item("c", "ok"), "d".into()).unwrap();
        wait_idle(&m).await;
        let s = m.snapshot();
        assert_eq!(s[0].state, JobState::Done(PathBuf::from("d").join("a.m4a")));
        assert_eq!(s[1].state, JobState::Failed("boom".into()));
        assert_eq!(s[2].state, JobState::Done(PathBuf::from("d").join("c.m4a")));
        assert!(changes.load(Ordering::SeqCst) >= 6);
    }

    #[tokio::test]
    async fn runs_up_to_the_slot_count_at_once() {
        let fake = Arc::new(Fake { started: AtomicUsize::new(0) });
        let m = DownloadManager::start_with(fake.clone(), &tokio::runtime::Handle::current(), 2, || {});
        let ids: Vec<u64> = (0..3).map(|i| m.enqueue(item(&format!("s{i}"), "slow"), "d".into()).unwrap()).collect();
        for _ in 0..200 {
            if fake.started.load(Ordering::SeqCst) >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(fake.started.load(Ordering::SeqCst), 2, "third job waits for a free slot");
        let running = m.snapshot().iter().filter(|j| matches!(j.state, JobState::Running(_))).count();
        assert_eq!(running, 2);
        // Freeing a slot lets the third start.
        m.cancel(ids[0]);
        for _ in 0..200 {
            if fake.started.load(Ordering::SeqCst) == 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(fake.started.load(Ordering::SeqCst), 3);
        for id in ids {
            m.cancel(id);
        }
        wait_idle(&m).await;
    }

    #[tokio::test]
    async fn priority_job_jumps_the_waiting_queue_but_not_the_running_one() {
        let (m, fake, _c) = manager();
        let first = m.enqueue(item("a", "slow"), "d".into()).unwrap();
        m.enqueue(item("b", "ok"), "d".into()).unwrap();
        m.enqueue(item("c", "ok"), "d".into()).unwrap();
        for _ in 0..200 {
            if fake.started.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // c is moved ahead of b; a keeps running.
        m.enqueue_next(item("c", "ok"), "d".into());
        let order: Vec<String> = m.snapshot().iter().map(|j| j.item.id.clone()).collect();
        assert_eq!(order, ["a", "c", "b"]);
        // A brand new priority job also goes before b.
        m.enqueue_next(item("d", "ok"), "d".into());
        let order: Vec<String> = m.snapshot().iter().map(|j| j.item.id.clone()).collect();
        assert_eq!(order, ["a", "d", "c", "b"]);
        m.cancel(first);
        wait_idle(&m).await;
    }

    #[tokio::test]
    async fn refuses_duplicates_while_active_but_allows_retry_after() {
        let (m, _f, _c) = manager();
        let id = m.enqueue(item("a", "slow"), "d".into()).unwrap();
        assert!(m.enqueue(item("a", "slow"), "d".into()).is_none());
        m.cancel(id);
        wait_idle(&m).await;
        assert!(m.enqueue(item("a", "ok"), "d".into()).is_some());
        wait_idle(&m).await;
    }

    #[tokio::test]
    async fn cancels_running_and_queued_jobs() {
        let (m, fake, _c) = manager();
        let running = m.enqueue(item("a", "slow"), "d".into()).unwrap();
        let queued = m.enqueue(item("b", "ok"), "d".into()).unwrap();
        // Wait until the slow job has started.
        for _ in 0..200 {
            if fake.started.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        m.cancel(queued);
        m.cancel(running);
        wait_idle(&m).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let s = m.snapshot();
        assert_eq!(s[0].state, JobState::Cancelled);
        assert_eq!(s[1].state, JobState::Cancelled);
        assert_eq!(fake.started.load(Ordering::SeqCst), 1, "cancelled queued job never starts");
    }

    #[tokio::test]
    async fn clear_removes_only_finished_jobs() {
        let (m, _f, _c) = manager();
        m.enqueue(item("a", "ok"), "d".into()).unwrap();
        wait_idle(&m).await;
        m.enqueue(item("b", "slow"), "d".into()).unwrap();
        m.clear_finished();
        let s = m.snapshot();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].item.id, "b");
        m.cancel(s[0].id);
    }
}
