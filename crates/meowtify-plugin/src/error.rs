use std::fmt;

/// Errors returned by the plugin runtime and package loaders.
#[derive(Debug)]
pub enum PluginError {
    Io(String),
    Manifest(String),
    Theme(String),
    Compile(String),
    Wasm(String),
    /// The plugin trapped or was marked failed earlier.
    Failed(String),
    /// The command queue is full.
    Busy,
    /// A call did not finish in time.
    Timeout,
    /// All plugin slots are in use.
    TooManyPlugins,
    /// The package has no wasm code (theme only).
    NoCode,
}

impl fmt::Display for PluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PluginError::Io(m) => write!(f, "io error: {m}"),
            PluginError::Manifest(m) => write!(f, "invalid manifest: {m}"),
            PluginError::Theme(m) => write!(f, "invalid theme: {m}"),
            PluginError::Compile(m) => write!(f, "compile failed: {m}"),
            PluginError::Wasm(m) => write!(f, "wasm error: {m}"),
            PluginError::Failed(m) => write!(f, "plugin failed: {m}"),
            PluginError::Busy => write!(f, "plugin queue is full"),
            PluginError::Timeout => write!(f, "plugin call timed out"),
            PluginError::TooManyPlugins => write!(f, "too many plugins running"),
            PluginError::NoCode => write!(f, "package has no code"),
        }
    }
}

impl std::error::Error for PluginError {}

#[allow(dead_code)] // used by the package installer, which is not written yet
pub(crate) fn io_err(context: &str, e: std::io::Error) -> PluginError {
    PluginError::Io(format!("{context}: {e}"))
}

/// Truncates `s` to at most `max` bytes on a char boundary.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}
