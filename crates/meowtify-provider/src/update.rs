//! Updates for Meowtify itself. The latest GitHub release holds the app and a `SHA256SUMS`
//! file. The app looks at the release, and only when the user agrees it downloads the new
//! program, checks it against the listed hash, and swaps it in.
//!
//! Everything goes through the same `curl` as the rest of the app, so the proxy setting
//! applies. Only links under the project's own release path are followed.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::ytdlp::{curl_file, curl_get, expected_hash, hash_file};
use crate::{ProviderError, Result};

const API: &str = "https://api.github.com/repos/jun-ro/meowtify/releases/latest";
const DOWNLOAD_PREFIX: &str = "https://github.com/jun-ro/meowtify/releases/download/";
/// The program inside a release. Only Windows builds are published.
const ASSET: &str = "meowtify-windows-x64.exe";
const SUMS: &str = "SHA256SUMS";

/// A release newer than the running app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub notes: String,
    exe_url: String,
    sums_url: String,
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
}

/// `1.2.3` or `v1.2.3` as numbers. Anything after a `-` or `+` is ignored.
fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let core = text.trim().trim_start_matches('v').split(['-', '+']).next()?;
    let mut it = core.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next().unwrap_or("0").parse().ok()?;
    let patch = it.next().unwrap_or("0").parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

fn link_ok(url: &str) -> bool {
    url.len() <= 400
        && url.starts_with(DOWNLOAD_PREFIX)
        && url[DOWNLOAD_PREFIX.len()..].chars().all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
        && !url.contains("..")
}

/// The release in `body`, when it is newer than `current` and has the files we need.
fn pick(body: &[u8], current: &str) -> Result<Option<Release>> {
    let r: ApiRelease = serde_json::from_slice(body).map_err(|e| ProviderError::Parse(e.to_string()))?;
    if r.draft || r.prerelease {
        return Ok(None);
    }
    let (Some(new), Some(old)) = (parse_version(&r.tag_name), parse_version(current)) else {
        return Err(ProviderError::Parse(format!("cannot read the version {:?}", r.tag_name)));
    };
    if new <= old {
        return Ok(None);
    }
    let find = |name: &str| r.assets.iter().find(|a| a.name == name).map(|a| a.browser_download_url.clone()).filter(|u| link_ok(u));
    let (Some(exe_url), Some(sums_url)) = (find(ASSET), find(SUMS)) else {
        return Err(ProviderError::Parse("the latest release has no program for this system".into()));
    };
    let notes: String = r.body.unwrap_or_default().chars().filter(|c| !c.is_control() || *c == '\n').take(1500).collect();
    Ok(Some(Release { version: r.tag_name.trim_start_matches('v').to_string(), notes, exe_url, sums_url }))
}

/// Asks GitHub for the latest release. `Ok(None)` means the app is current. Other systems than
/// Windows never get an update, because no program is published for them.
pub async fn check(current: &str) -> Result<Option<Release>> {
    if !cfg!(windows) {
        return Ok(None);
    }
    let body = curl_get(API, "Meowtify-updater", 15).await?;
    pick(&body, current)
}

/// Downloads `release`, checks its hash, and puts it in place of the running program. The old
/// program stays beside it as `.old` until the next start. Returns the path to launch.
pub async fn install(release: &Release) -> Result<PathBuf> {
    let exe = std::env::current_exe().map_err(|e| ProviderError::Io(e.to_string()))?;
    let part = side(&exe, "new");
    let old = side(&exe, "old");
    let _ = std::fs::remove_file(&part);

    let sums = curl_get(&release.sums_url, "Meowtify-updater", 30).await?;
    let sums = String::from_utf8_lossy(&sums);
    let expected = expected_hash(&sums, ASSET).ok_or_else(|| ProviderError::Parse(format!("no {ASSET} entry in {SUMS}")))?;

    let done = async {
        curl_file(&release.exe_url, &part).await?;
        if hash_file(&part).await.as_deref() != Some(expected.as_str()) {
            return Err(ProviderError::Tool("the download did not match its checksum".into()));
        }
        // A running program cannot be overwritten on Windows, but it can be renamed.
        let _ = std::fs::remove_file(&old);
        std::fs::rename(&exe, &old).map_err(|e| ProviderError::Io(e.to_string()))?;
        if let Err(e) = std::fs::rename(&part, &exe) {
            let _ = std::fs::rename(&old, &exe);
            return Err(ProviderError::Io(e.to_string()));
        }
        Ok(())
    }
    .await;
    let _ = std::fs::remove_file(&part);
    done.map(|_| exe)
}

/// Removes the program left behind by the last update. Call once at start.
pub fn clean_up() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(side(&exe, "old"));
        let _ = std::fs::remove_file(side(&exe, "new"));
    }
}

/// `meowtify.exe` becomes `meowtify.exe.old`, in the same folder.
fn side(exe: &Path, ext: &str) -> PathBuf {
    let mut name = exe.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".");
    name.push(ext);
    exe.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, extra: &str) -> String {
        format!(
            r#"{{"tag_name":"{tag}","body":"Notes","assets":[
            {{"name":"{ASSET}","browser_download_url":"{DOWNLOAD_PREFIX}{tag}/{ASSET}"}},
            {{"name":"{SUMS}","browser_download_url":"{DOWNLOAD_PREFIX}{tag}/{SUMS}"}}]{extra}}}"#
        )
    }

    #[test]
    fn reads_versions() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("0.1"), Some((0, 1, 0)));
        assert_eq!(parse_version("2.0.0-beta.1"), Some((2, 0, 0)));
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version("latest"), None);
        assert!(parse_version("0.10.0") > parse_version("0.9.9"));
    }

    #[test]
    fn newer_release_is_offered() {
        let r = pick(release("v0.2.0", "").as_bytes(), "0.1.0").unwrap().unwrap();
        assert_eq!(r.version, "0.2.0");
        assert_eq!(r.notes, "Notes");
    }

    #[test]
    fn same_or_older_release_is_not() {
        assert_eq!(pick(release("v0.1.0", "").as_bytes(), "0.1.0").unwrap(), None);
        assert_eq!(pick(release("v0.0.9", "").as_bytes(), "0.1.0").unwrap(), None);
    }

    #[test]
    fn drafts_and_prereleases_are_skipped() {
        assert_eq!(pick(release("v9.0.0", r#","prerelease":true"#).as_bytes(), "0.1.0").unwrap(), None);
        assert_eq!(pick(release("v9.0.0", r#","draft":true"#).as_bytes(), "0.1.0").unwrap(), None);
    }

    #[test]
    fn foreign_links_are_refused() {
        let evil = release("v0.2.0", "").replace(DOWNLOAD_PREFIX, "https://example.com/");
        assert!(pick(evil.as_bytes(), "0.1.0").is_err());
        assert!(!link_ok(&format!("{DOWNLOAD_PREFIX}v1/../../x")));
        assert!(!link_ok("https://github.com/someone-else/meowtify/releases/download/v1/x"));
        assert!(link_ok(&format!("{DOWNLOAD_PREFIX}v1.0.0/{ASSET}")));
    }

    #[test]
    fn a_release_without_the_program_is_an_error() {
        let body = r#"{"tag_name":"v0.2.0","assets":[]}"#;
        assert!(pick(body.as_bytes(), "0.1.0").is_err());
    }

    #[test]
    fn side_files_stay_in_the_same_folder() {
        let p = side(Path::new(r"C:\apps\meowtify.exe"), "old");
        assert_eq!(p, Path::new(r"C:\apps\meowtify.exe.old"));
    }
}
