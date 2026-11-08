//! Sandboxed WebAssembly plugin host.
//!
//! The shipped app links only the Wasmtime runtime. Plugins arrive as `.wasm`,
//! are turned into `.cwasm` at install time by `meowtify-plugin-compile` using
//! [`engine_config`], and are loaded here with [`Runtime::load`].
//!
//! Plugins are untrusted. Each one gets a memory cap, a per-call epoch
//! deadline, and only the host functions its manifest permits.

mod bindings;
mod error;
mod manifest;
mod runtime;
mod url;

pub use bindings::meowtify::plugin::types::{
    ErrorKind, HostError, HttpResponse, LogLevel, LyricLine, Lyrics, Track,
};
pub use error::PluginError;
pub use manifest::{Manifest, Permissions};
pub use runtime::{
    engine_config, Control, LoadedPlugin, Runtime, Services, TrustedArtifact, CALL_DEADLINE_TICKS, MAX_PLUGINS,
    MEM_CAP, TICK, TOTAL_MEM_CAP,
};
pub use url::{host_allowed, normalize_host_pattern, valid_hostname, Url};
