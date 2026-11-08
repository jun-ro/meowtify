//! Wasmtime engine, plugin loading, host functions and call containment.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{
    Config, Engine, InstanceAllocationStrategy, PoolingAllocationConfig, ResourceLimiter, Store, StoreLimits,
    StoreLimitsBuilder,
};

use crate::bindings::meowtify::plugin::{host, types};
use crate::bindings::Plugin;
use crate::error::{truncate, PluginError};
use crate::manifest::{Manifest, Permissions};
use crate::url::{host_allowed, Url};
use crate::{ErrorKind, HostError, HttpResponse, LogLevel, Lyrics, Track};

/// Memory cap per plugin (linear memory). Counts against the 45 MB plugin budget.
pub const MEM_CAP: usize = 8 << 20;
/// Linear memory of all loaded plugins together. Counts against the 45 MB plugin budget.
pub const TOTAL_MEM_CAP: usize = 40 << 20;
/// Plugins that can be loaded at once.
pub const MAX_PLUGINS: usize = 8;
/// Epoch tick length. The ticker thread bumps the engine epoch this often.
pub const TICK: Duration = Duration::from_millis(10);
/// Ticks a single call may take: 30 x 10 ms = 300 ms.
pub const CALL_DEADLINE_TICKS: u64 = 30;

const MAX_LOG: usize = 1024;
const MAX_KEY: usize = 128;
const MAX_VALUE: usize = 64 << 10;

/// Engine settings shared by the app and the install-time compiler.
///
/// A `.cwasm` only loads into an engine with the same settings, so both sides
/// must call this and nothing else.
pub fn engine_config() -> Config {
    let mut c = Config::new();
    c.epoch_interruption(true);
    // Small address-space reservation: explicit bounds checks past the cap
    // instead of a 4 GiB reservation per memory.
    c.memory_reservation(MEM_CAP as u64);
    c.memory_guard_size(64 << 10);
    c.memory_reservation_for_growth(0);

    let mut p = PoolingAllocationConfig::new();
    p.total_component_instances(MAX_PLUGINS as u32);
    p.total_core_instances(4 * MAX_PLUGINS as u32);
    p.total_memories(4 * MAX_PLUGINS as u32);
    p.total_tables(4 * MAX_PLUGINS as u32);
    p.max_memory_size(MEM_CAP);
    p.table_elements(1000);
    p.max_unused_warm_slots(2);
    c.allocation_strategy(InstanceAllocationStrategy::Pooling(p));
    c
}

/// Playback and queue actions a plugin may request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    Play,
    Pause,
    Next,
    Previous,
    Seek(u32),
    SetVolume(u8),
    QueueAdd(String),
    QueueRemove(String),
}

fn unavailable() -> HostError {
    HostError { kind: ErrorKind::Unavailable, message: "not available".into() }
}

fn denied(what: &str) -> HostError {
    HostError { kind: ErrorKind::Denied, message: format!("permission denied: {what}") }
}

fn invalid(what: &str) -> HostError {
    HostError { kind: ErrorKind::Invalid, message: what.to_string() }
}

/// What the app offers to plugins. Called on the thread that runs the plugin,
/// after the permission check. Every method has a safe default.
pub trait Services: Send + Sync + 'static {
    fn log(&self, _plugin: &str, _level: LogLevel, _message: &str) {}
    fn control(&self, _plugin: &str, _action: Control) -> Result<(), HostError> {
        Err(unavailable())
    }
    fn now_playing(&self) -> Result<Option<Track>, HostError> {
        Err(unavailable())
    }
    fn queue(&self) -> Result<Vec<Track>, HostError> {
        Err(unavailable())
    }
    /// `url` is already parsed and checked against the plugin's allowlist.
    fn fetch(&self, _plugin: &str, _url: &Url) -> Result<HttpResponse, HostError> {
        Err(unavailable())
    }
    fn storage_get(&self, _plugin: &str, _key: &str) -> Result<Option<Vec<u8>>, HostError> {
        Err(unavailable())
    }
    fn storage_set(&self, _plugin: &str, _key: &str, _value: &[u8]) -> Result<(), HostError> {
        Err(unavailable())
    }
    fn storage_delete(&self, _plugin: &str, _key: &str) -> Result<(), HostError> {
        Err(unavailable())
    }
}

/// A compiled plugin file the app itself produced. Loading deserializes native code from
/// it without checks, so a path from anywhere else must never reach [`Runtime::load`].
pub struct TrustedArtifact(PathBuf);

impl TrustedArtifact {
    /// A file inside `root`, the folder where the install step writes compiled plugins.
    /// Refuses anything else: other folders, links that lead out, and non-files.
    pub fn in_install_dir(root: &Path, file: &Path) -> Result<TrustedArtifact, PluginError> {
        let root = root.canonicalize().map_err(|e| PluginError::Io(e.to_string()))?;
        let file = file.canonicalize().map_err(|e| PluginError::Io(e.to_string()))?;
        if !file.starts_with(&root) || !file.is_file() {
            return Err(PluginError::Io("the compiled plugin is not inside the install folder".into()));
        }
        Ok(TrustedArtifact(file))
    }

    /// Declares `file` trusted without checking where it is.
    ///
    /// # Safety
    /// The file must have been produced by `meowtify-plugin-compile` with this build's
    /// [`engine_config`] and must not be writable by anyone who is not trusted: Wasmtime
    /// runs its contents as native code.
    pub unsafe fn assume_trusted(file: PathBuf) -> TrustedArtifact {
        TrustedArtifact(file)
    }
}

/// Memory limits of one plugin, which also count against the budget of all plugins.
struct PluginLimits {
    each: StoreLimits,
    total: Arc<AtomicUsize>,
    /// Bytes of linear memory this plugin holds.
    mine: usize,
}

impl ResourceLimiter for PluginLimits {
    fn memory_growing(&mut self, current: usize, desired: usize, maximum: Option<usize>) -> wasmtime::Result<bool> {
        if !self.each.memory_growing(current, desired, maximum)? {
            return Ok(false);
        }
        let grow = desired.saturating_sub(current);
        let took = self.total.fetch_update(Ordering::AcqRel, Ordering::Acquire, |t| (t + grow <= TOTAL_MEM_CAP).then_some(t + grow));
        if took.is_err() {
            return Ok(false);
        }
        self.mine += grow;
        Ok(true)
    }

    fn table_growing(&mut self, current: usize, desired: usize, maximum: Option<usize>) -> wasmtime::Result<bool> {
        self.each.table_growing(current, desired, maximum)
    }
}

impl Drop for PluginLimits {
    fn drop(&mut self) {
        self.total.fetch_sub(self.mine, Ordering::AcqRel);
    }
}

/// The thread that advances the engine epoch. Shared by the runtime and every plugin it
/// loaded, so a plugin that outlives the runtime still has its deadlines.
struct Ticker {
    stop: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for Ticker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.lock().unwrap().take() {
            let _ = t.join();
        }
    }
}

struct HostState {
    limits: PluginLimits,
    perms: Permissions,
    plugin_id: Arc<str>,
    services: Arc<dyn Services>,
}

impl HostState {
    fn control(&self, action: Control) -> Result<(), HostError> {
        if !self.perms.control {
            return Err(denied("control"));
        }
        self.services.control(&self.plugin_id, action)
    }

    fn storage_key(&self, key: &str) -> Result<(), HostError> {
        if !self.perms.storage {
            return Err(denied("storage"));
        }
        if key.is_empty() || key.len() > MAX_KEY {
            return Err(invalid("key must be 1-128 bytes"));
        }
        Ok(())
    }
}

impl types::Host for HostState {}

impl host::Host for HostState {
    fn log(&mut self, level: LogLevel, message: String) {
        self.services.log(&self.plugin_id, level, &truncate(&message, MAX_LOG));
    }

    fn play(&mut self) -> Result<(), HostError> {
        self.control(Control::Play)
    }

    fn pause(&mut self) -> Result<(), HostError> {
        self.control(Control::Pause)
    }

    fn next(&mut self) -> Result<(), HostError> {
        self.control(Control::Next)
    }

    fn previous(&mut self) -> Result<(), HostError> {
        self.control(Control::Previous)
    }

    fn seek(&mut self, position_ms: u32) -> Result<(), HostError> {
        self.control(Control::Seek(position_ms))
    }

    fn set_volume(&mut self, percent: u8) -> Result<(), HostError> {
        if !self.perms.control {
            return Err(denied("control"));
        }
        if percent > 100 {
            return Err(invalid("volume is 0-100"));
        }
        self.control(Control::SetVolume(percent))
    }

    fn now_playing(&mut self) -> Result<Option<Track>, HostError> {
        self.services.now_playing()
    }

    fn queue(&mut self) -> Result<Vec<Track>, HostError> {
        self.services.queue()
    }

    fn queue_add(&mut self, track_id: String) -> Result<(), HostError> {
        self.control(Control::QueueAdd(track_id))
    }

    fn queue_remove(&mut self, track_id: String) -> Result<(), HostError> {
        self.control(Control::QueueRemove(track_id))
    }

    fn fetch(&mut self, url: String) -> Result<HttpResponse, HostError> {
        if self.perms.fetch.is_empty() {
            return Err(denied("fetch"));
        }
        let url = Url::parse(&url).map_err(|e| invalid(&e))?;
        if !host_allowed(&self.perms.fetch, &url.host) {
            return Err(denied("host not in allowlist"));
        }
        self.services.fetch(&self.plugin_id, &url)
    }

    fn storage_get(&mut self, key: String) -> Result<Option<Vec<u8>>, HostError> {
        self.storage_key(&key)?;
        self.services.storage_get(&self.plugin_id, &key)
    }

    fn storage_set(&mut self, key: String, value: Vec<u8>) -> Result<(), HostError> {
        self.storage_key(&key)?;
        if value.len() > MAX_VALUE {
            return Err(HostError { kind: ErrorKind::TooLarge, message: "value over 64 KiB".into() });
        }
        self.services.storage_set(&self.plugin_id, &key, &value)
    }

    fn storage_delete(&mut self, key: String) -> Result<(), HostError> {
        self.storage_key(&key)?;
        self.services.storage_delete(&self.plugin_id, &key)
    }
}

/// Owns the engine and the epoch ticker. Create one per app.
pub struct Runtime {
    engine: Engine,
    services: Arc<dyn Services>,
    loaded: Arc<AtomicUsize>,
    /// Linear memory held by all loaded plugins together.
    memory: Arc<AtomicUsize>,
    ticker: Arc<Ticker>,
}

impl Runtime {
    pub fn new(services: Arc<dyn Services>) -> Result<Runtime, PluginError> {
        let engine = Engine::new(&engine_config()).map_err(|e| PluginError::Wasm(first_line(&e)))?;
        let stop = Arc::new(AtomicBool::new(false));
        let ticker = {
            let (engine, stop) = (engine.clone(), stop.clone());
            std::thread::Builder::new()
                .name("plugin-epoch".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(TICK);
                        engine.increment_epoch();
                    }
                })
                .map_err(|e| PluginError::Io(e.to_string()))?
        };
        let ticker = Arc::new(Ticker { stop, thread: Mutex::new(Some(ticker)) });
        Ok(Runtime { engine, services, loaded: Arc::new(AtomicUsize::new(0)), memory: Arc::new(AtomicUsize::new(0)), ticker })
    }

    /// Loads a precompiled `.cwasm`. A file made by another Wasmtime version
    /// or another [`engine_config`] fails with [`PluginError::Wasm`]; the
    /// caller should recompile from the kept `.wasm`.
    pub fn load(&self, manifest: Manifest, cwasm: &TrustedArtifact) -> Result<LoadedPlugin, PluginError> {
        if manifest.wasm.is_none() {
            return Err(PluginError::NoCode);
        }
        let reserved = self
            .loaded
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| (n < MAX_PLUGINS).then_some(n + 1))
            .is_ok();
        if !reserved {
            return Err(PluginError::TooManyPlugins);
        }
        // From here the slot is released by the guard's Drop on any error.
        let slot = Slot(self.loaded.clone());

        // SAFETY: `TrustedArtifact` can only be made for a file the install step wrote (or by
        // a caller who took responsibility for that with `assume_trusted`).
        let component = unsafe { Component::deserialize_file(&self.engine, &cwasm.0) }
            .map_err(|e| PluginError::Wasm(first_line(&e)))?;

        let mut linker: Linker<HostState> = Linker::new(&self.engine);
        Plugin::add_to_linker::<_, HasSelf<_>>(&mut linker, |s: &mut HostState| s)
            .map_err(|e| PluginError::Wasm(first_line(&e)))?;
        // Guests may import WASI. Give them traps, not capabilities.
        linker
            .define_unknown_imports_as_traps(&component)
            .map_err(|e| PluginError::Wasm(first_line(&e)))?;

        let mut store = Store::new(
            &self.engine,
            HostState {
                limits: PluginLimits {
                    each: StoreLimitsBuilder::new().memory_size(MEM_CAP).instances(4).memories(4).tables(4).build(),
                    total: self.memory.clone(),
                    mine: 0,
                },
                perms: manifest.permissions.clone(),
                plugin_id: manifest.id.as_str().into(),
                services: self.services.clone(),
            },
        );
        store.limiter(|h| &mut h.limits);
        store.set_epoch_deadline(CALL_DEADLINE_TICKS);

        let plugin = Plugin::instantiate(&mut store, &component, &linker)
            .map_err(|e| PluginError::Wasm(first_line(&e)))?;
        Ok(LoadedPlugin { manifest, store, plugin, failed: None, _slot: slot, _ticker: self.ticker.clone() })
    }
}

struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// One running plugin instance. Calls block the caller, so run them off the UI
/// thread. A trap, timeout or memory overrun marks the plugin failed for good.
pub struct LoadedPlugin {
    manifest: Manifest,
    store: Store<HostState>,
    plugin: Plugin,
    failed: Option<String>,
    _slot: Slot,
    /// Keeps the epoch ticker running for as long as this plugin lives.
    _ticker: Arc<Ticker>,
}

impl LoadedPlugin {
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Why the plugin stopped working, if it did.
    pub fn failure(&self) -> Option<&str> {
        self.failed.as_deref()
    }

    pub fn on_track_change(&mut self, track: &Track) -> Result<(), PluginError> {
        self.call(|p, s| p.call_on_track_change(s, track))
    }

    pub fn on_progress(&mut self, position_ms: u32) -> Result<(), PluginError> {
        self.call(|p, s| p.call_on_progress(s, position_ms))
    }

    pub fn on_queue_change(&mut self, queue: &[Track]) -> Result<(), PluginError> {
        self.call(|p, s| p.call_on_queue_change(s, queue))
    }

    pub fn get_lyrics(&mut self, track: &Track) -> Result<Option<Lyrics>, PluginError> {
        self.call(|p, s| p.call_get_lyrics(s, track))
    }

    fn call<R>(
        &mut self,
        f: impl FnOnce(&Plugin, &mut Store<HostState>) -> wasmtime::Result<R>,
    ) -> Result<R, PluginError> {
        if let Some(m) = &self.failed {
            return Err(PluginError::Failed(m.clone()));
        }
        self.store.set_epoch_deadline(CALL_DEADLINE_TICKS);
        f(&self.plugin, &mut self.store).map_err(|e| {
            let msg = truncate(&first_line(&e), 200);
            self.failed = Some(msg.clone());
            PluginError::Failed(msg)
        })
    }
}

fn first_line(e: &wasmtime::Error) -> String {
    format!("{e:?}").lines().next().unwrap_or("").to_string()
}
