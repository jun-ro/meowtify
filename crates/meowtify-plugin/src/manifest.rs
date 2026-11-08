//! Plugin manifest (`plugin.toml`). The user approves the permissions at install.

use serde::Deserialize;

use crate::error::PluginError;
use crate::url::normalize_host_pattern;

const MAX_ID: usize = 64;
const MAX_NAME: usize = 64;
const MAX_FETCH_HOSTS: usize = 16;

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Permissions {
    /// May call play, pause, next, previous, seek, volume and queue edits.
    pub control: bool,
    /// May use the per-plugin key-value storage.
    pub storage: bool,
    /// Hosts `fetch` may reach. `example.com` or `*.example.com`.
    pub fetch: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: String,
    /// Path of the `.wasm` file relative to the manifest. Absent for theme-only packages.
    pub wasm: Option<String>,
    #[serde(default)]
    pub permissions: Permissions,
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Manifest, PluginError> {
        let mut m: Manifest = toml::from_str(text).map_err(|e| PluginError::Manifest(e.to_string()))?;
        m.validate()?;
        Ok(m)
    }

    fn validate(&mut self) -> Result<(), PluginError> {
        let bad = |m: &str| Err(PluginError::Manifest(m.to_string()));
        if self.id.is_empty()
            || self.id.len() > MAX_ID
            || !self.id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        {
            return bad("id must be 1-64 chars of a-z, 0-9, '-' or '_'");
        }
        if self.name.trim().is_empty() || self.name.len() > MAX_NAME {
            return bad("name must be 1-64 bytes");
        }
        if self.version.trim().is_empty() || self.version.len() > 32 {
            return bad("version must be 1-32 bytes");
        }
        if let Some(w) = &self.wasm {
            let p = std::path::Path::new(w);
            let escapes = p.is_absolute()
                || p.components().any(|c| !matches!(c, std::path::Component::Normal(_)));
            if w.is_empty() || escapes {
                return bad("wasm must be a relative path inside the package");
            }
        }
        if self.permissions.fetch.len() > MAX_FETCH_HOSTS {
            return bad("too many fetch hosts");
        }
        for h in &mut self.permissions.fetch {
            *h = normalize_host_pattern(h).map_err(PluginError::Manifest)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
id = "lyrics"
name = "Lyrics"
version = "0.1.0"
wasm = "plugin.wasm"

[permissions]
storage = true
fetch = ["LRCLIB.net", "*.example.com"]
"#;

    #[test]
    fn parses_and_normalizes() {
        let m = Manifest::parse(GOOD).unwrap();
        assert_eq!(m.id, "lyrics");
        assert!(m.permissions.storage);
        assert!(!m.permissions.control);
        assert_eq!(m.permissions.fetch, ["lrclib.net", "*.example.com"]);
    }

    #[test]
    fn permissions_default_to_none() {
        let m = Manifest::parse("id='a'\nname='A'\nversion='1'\n").unwrap();
        assert_eq!(m.permissions, Permissions::default());
        assert!(m.wasm.is_none());
    }

    #[test]
    fn rejects_bad_input() {
        for text in [
            "id='A'\nname='A'\nversion='1'",
            "id='a'\nname='A'\nversion='1'\nwasm='../x.wasm'",
            "id='a'\nname='A'\nversion='1'\nwasm='/x.wasm'",
            "id='a'\nname='A'\nversion='1'\n[permissions]\nfetch=['bad host']",
            "id='a'\nname='A'\nversion='1'\nextra=1",
            "id='a'\nname='A'",
        ] {
            assert!(Manifest::parse(text).is_err(), "should reject: {text}");
        }
    }
}
