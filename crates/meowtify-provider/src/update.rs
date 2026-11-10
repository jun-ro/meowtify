//! Updates for Meowtify itself. The latest GitHub release holds the app and a `SHA256SUMS`
//! file. The app looks at the release, and only when the user agrees it downloads the new
//! program, checks it against the listed hash, and swaps it in.
//!
//! Everything goes through the same `curl` as the rest of the app, so the proxy setting
//! applies. Only links under the project's own release path are followed.
//!
//! Checks after the first one are conditional: the ETag of the last answer is kept, and a
//! `304 Not Modified` reuses that answer. GitHub does not count a 304 against the hourly limit
//! of unauthenticated requests, so a quiet re-check every few minutes is cheap.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;

use crate::youtube::plain_command;
use crate::ytdlp::{curl_file, curl_get, expected_hash, hash_file};
use crate::{ProviderError, Result};

const API: &str = "https://api.github.com/repos/jun-ro/meowtify/releases/latest";
const DOWNLOAD_PREFIX: &str = "https://github.com/jun-ro/meowtify/releases/download/";
/// The app inside a release. Only Windows builds are published.
const ASSET: &str = "meowtify-windows-x64.exe";
/// The command line tool inside a release. Releases before the tool existed lack it.
const CLI_ASSET: &str = "meowtify-cli-windows-x64.exe";
const SUMS: &str = "SHA256SUMS";

/// The error text for a 403 or 429: GitHub's hourly limit is reached. Quiet checks stay quiet,
/// and the app waits `RATE_LIMIT_PAUSE` before trying again.
const RATE_LIMIT_TEXT: &str = "GitHub asked Meowtify to wait before checking for updates again";

/// How often a running app may quietly re-check for a release.
pub const RECHECK_EVERY: Duration = Duration::from_secs(600);
/// The longer pause after GitHub says the rate limit is reached.
pub const RATE_LIMIT_PAUSE: Duration = Duration::from_secs(1800);


/// A program a release can hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Program {
    /// The app with the window.
    App,
    /// The command line tool, for when the window does not work.
    Cli,
}

impl Program {
    fn asset(self) -> &'static str {
        match self {
            Program::App => ASSET,
            Program::Cli => CLI_ASSET,
        }
    }
}

/// A release newer than the running app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub notes: String,
    exe_url: String,
    exe_size: Option<u64>,
    cli_url: Option<String>,
    cli_size: Option<u64>,
    sums_url: String,
}

impl Release {
    fn url(&self, program: Program) -> Option<&str> {
        match program {
            Program::App => Some(&self.exe_url),
            Program::Cli => self.cli_url.as_deref(),
        }
    }

    /// The size GitHub lists for that program, used as the download's total.
    fn size(&self, program: Program) -> Option<u64> {
        match program {
            Program::App => self.exe_size,
            Program::Cli => self.cli_size,
        }
    }
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
    #[serde(default)]
    size: Option<u64>,
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

/// The release in `body`, when it is newer than `current` (or `force` is set) and has the files
/// we need.
fn pick(body: &[u8], current: &str, force: bool) -> Result<Option<Release>> {
    let r: ApiRelease = serde_json::from_slice(body).map_err(|e| ProviderError::Parse(e.to_string()))?;
    if r.draft || r.prerelease {
        return Ok(None);
    }
    let (Some(new), Some(old)) = (parse_version(&r.tag_name), parse_version(current)) else {
        return Err(ProviderError::Parse(format!("cannot read the version {:?}", r.tag_name)));
    };
    if new <= old && !force {
        return Ok(None);
    }
    let find = |name: &str| {
        r.assets
            .iter()
            .find(|a| a.name == name)
            .map(|a| (a.browser_download_url.clone(), a.size))
            .filter(|(u, _)| link_ok(u))
    };
    let (Some((exe_url, exe_size)), Some((sums_url, _))) = (find(ASSET), find(SUMS)) else {
        return Err(ProviderError::Parse("the latest release has no program for this system".into()));
    };
    let notes: String = r.body.unwrap_or_default().chars().filter(|c| !c.is_control() || *c == '\n').take(1500).collect();
    let (cli_url, cli_size) = match find(CLI_ASSET) {
        Some((url, size)) => (Some(url), size),
        None => (None, None),
    };
    Ok(Some(Release { version: r.tag_name.trim_start_matches('v').to_string(), notes, exe_url, exe_size, cli_url, cli_size, sums_url }))
}

/// The last answer that had an ETag, kept so a conditional request can reuse it.
#[derive(Debug)]
struct Cached {
    etag: String,
    body: Vec<u8>,
}

static CACHE: Mutex<Option<Cached>> = Mutex::new(None);

/// What a conditional request's status means for the stored answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// 200: the body holds the latest release.
    Fresh,
    /// 304: the stored body is still the latest.
    Unchanged,
    /// 403 or 429: GitHub's rate limit is reached.
    RateLimited,
    /// Anything else: no usable answer.
    Failed,
}

fn classify(status: u16) -> Answer {
    match status {
        200 => Answer::Fresh,
        304 => Answer::Unchanged,
        403 | 429 => Answer::RateLimited,
        _ => Answer::Failed,
    }
}

/// The status and the ETag of the last response in a `curl --dump-header` file. A redirect
/// writes several header blocks; only the last one answers the request.
fn read_head(dump: &str) -> (Option<u16>, Option<String>) {
    let mut status = None;
    let mut etag = None;
    for line in dump.lines() {
        let line = line.trim_end_matches('\r');
        if line.get(..5).is_some_and(|s| s.eq_ignore_ascii_case("HTTP/")) {
            status = line.split_whitespace().nth(1).and_then(|s| s.parse().ok());
            etag = None;
        } else if let Some((key, value)) = line.split_once(':') {
            if key.trim().eq_ignore_ascii_case("etag") {
                etag = Some(value.trim().to_string());
            }
        }
    }
    (status, etag)
}

/// The body a 304 points back to. One that arrives with nothing stored is an error.
fn reuse(cache: Option<&Cached>) -> Result<&[u8]> {
    cache
        .map(|c| c.body.as_slice())
        .ok_or_else(|| ProviderError::Parse("GitHub said nothing changed, but nothing was stored".into()))
}

/// How long before the next quiet check: the ordinary interval, or the longer pause after the
/// rate limit. Pure, so the schedule is tested without a network.
pub fn wait_after(rate_limited: bool) -> Duration {
    if rate_limited {
        RATE_LIMIT_PAUSE
    } else {
        RECHECK_EVERY
    }
}

/// True when `e` is GitHub's rate-limit refusal. Quiet checks skip without a toast then.
pub fn rate_limited(e: &ProviderError) -> bool {
    matches!(e, ProviderError::Tool(m) if m == RATE_LIMIT_TEXT)
}

/// One answer from GitHub, headers included.
struct Fetched {
    status: u16,
    etag: Option<String>,
    body: Vec<u8>,
}

/// Asks the API for the latest release, with `If-None-Match` when `etag` is known. `curl`
/// writes its headers to a temporary file, because the status and ETag are what matter.
async fn fetch(etag: Option<&str>) -> Result<Fetched> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dump = std::env::temp_dir().join(format!("meowtify-update-{}-{}.head", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    let mut c = plain_command(Path::new(if cfg!(windows) { "curl.exe" } else { "curl" }));
    crate::net::apply(&mut c);
    c.args([
        "--location",
        "--silent",
        "--show-error",
        "--compressed",
        "--max-time",
        "15",
        "--user-agent",
        "Meowtify-updater",
        "--dump-header",
    ])
    .arg(&dump);
    if let Some(tag) = etag {
        c.arg("--header").arg(format!("If-None-Match: {tag}"));
    }
    c.arg(API).stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = tokio::time::timeout(Duration::from_secs(30), c.output())
        .await
        .map_err(|_| ProviderError::Tool("the update check took too long".into()))?
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => ProviderError::Tool("curl was not found".into()),
            _ => ProviderError::Io(e.to_string()),
        })?;
    let head = std::fs::read_to_string(&dump).unwrap_or_default();
    let _ = std::fs::remove_file(&dump);
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(ProviderError::Tool(if msg.is_empty() { "the update check failed".into() } else { msg }));
    }
    let (status, etag) = read_head(&head);
    let status = status.ok_or_else(|| ProviderError::Parse("GitHub sent no status line".into()))?;
    Ok(Fetched { status, etag, body: out.stdout })
}

/// Asks GitHub for the latest release. `Ok(None)` means the app is current. Other systems than
/// Windows never get an update, because no program is published for them.
pub async fn check(current: &str) -> Result<Option<Release>> {
    check_with(current, false).await
}

/// Like [`check`]. With `force` the latest release is returned even when it is not newer, so a
/// damaged install can be put back.
pub async fn check_with(current: &str, force: bool) -> Result<Option<Release>> {
    if !cfg!(windows) {
        return Ok(None);
    }
    let etag = CACHE.lock().unwrap().as_ref().map(|c| c.etag.clone());
    let got = fetch(etag.as_deref()).await?;
    let body = match classify(got.status) {
        Answer::Fresh => {
            let body = got.body;
            *CACHE.lock().unwrap() = got.etag.map(|etag| Cached { etag, body: body.clone() });
            body
        }
        Answer::Unchanged => {
            let cache = CACHE.lock().unwrap();
            reuse(cache.as_ref())?.to_vec()
        }
        Answer::RateLimited => return Err(ProviderError::Tool(RATE_LIMIT_TEXT.into())),
        Answer::Failed => return Err(ProviderError::Tool(format!("GitHub answered {}.", got.status))),
    };
    pick(&body, current, force)
}

/// Downloads `release`, checks its hash, and puts it in place of the running program. The old
/// program stays beside it as `.old` until the next start. Returns the path to launch.
pub async fn install(release: &Release) -> Result<PathBuf> {
    install_with_progress(release, |_, _| {}).await
}

/// Like [`install`], reporting the download of the program: bytes so far, and the total when
/// GitHub lists the file's size.
pub async fn install_with_progress<F>(release: &Release, progress: F) -> Result<PathBuf>
where
    F: Fn(u64, Option<u64>) + Send + Sync,
{
    let exe = std::env::current_exe().map_err(|e| ProviderError::Io(e.to_string()))?;
    install_program_with_progress(release, Program::App, &exe, progress).await?;
    Ok(exe)
}

/// Downloads `program` from `release`, checks its hash, and puts it in place of the file at
/// `exe`. That file may be running: it is renamed to `.old`, which the program removes at its
/// next start.
pub async fn install_program(release: &Release, program: Program, exe: &Path) -> Result<()> {
    install_program_with_progress(release, program, exe, |_, _| {}).await
}

/// Like [`install_program`], reporting the download's progress.
pub async fn install_program_with_progress<F>(release: &Release, program: Program, exe: &Path, progress: F) -> Result<()>
where
    F: Fn(u64, Option<u64>) + Send + Sync,
{
    let asset = program.asset();
    let url = release.url(program).ok_or_else(|| ProviderError::Parse(format!("the release has no {asset}")))?;
    let part = side(exe, "new");
    let old = side(exe, "old");
    let _ = std::fs::remove_file(&part);

    let sums = curl_get(&release.sums_url, "Meowtify-updater", 30).await?;
    let sums = String::from_utf8_lossy(&sums);
    let expected = expected_hash(&sums, asset).ok_or_else(|| ProviderError::Parse(format!("no {asset} entry in {SUMS}")))?;

    let done = async {
        download(url, &part, release.size(program), &progress).await?;
        if hash_file(&part).await.as_deref() != Some(expected.as_str()) {
            return Err(ProviderError::Tool("the download did not match its checksum".into()));
        }
        // A running program cannot be overwritten on Windows, but it can be renamed.
        let _ = std::fs::remove_file(&old);
        // A missing file (a damaged install) has nothing to move aside.
        let moved = exe.exists();
        if moved {
            std::fs::rename(exe, &old).map_err(|e| ProviderError::Io(e.to_string()))?;
        }
        if let Err(e) = std::fs::rename(&part, exe) {
            if moved {
                let _ = std::fs::rename(&old, exe);
            }
            return Err(ProviderError::Io(e.to_string()));
        }
        Ok(())
    }
    .await;
    let _ = std::fs::remove_file(&part);
    done
}

/// Downloads `url` to `dest`, calling `progress` with the file's growing size about every
/// 150 ms, against `total` when it is known. `curl` writes no usable progress of its own.
async fn download<F>(url: &str, dest: &Path, total: Option<u64>, progress: &F) -> Result<()>
where
    F: Fn(u64, Option<u64>) + Send + Sync,
{
    progress(0, total);
    let transfer = curl_file(url, dest);
    tokio::pin!(transfer);
    loop {
        tokio::select! {
            done = &mut transfer => {
                done?;
                let size = tokio::fs::metadata(dest).await.map(|m| m.len()).unwrap_or(0);
                progress(total.unwrap_or(size), total);
                return Ok(());
            }
            _ = tokio::time::sleep(Duration::from_millis(150)) => {
                let size = tokio::fs::metadata(dest).await.map(|m| m.len()).unwrap_or(0);
                progress(size, total);
            }
        }
    }
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
        let r = pick(release("v0.2.0", "").as_bytes(), "0.1.0", false).unwrap().unwrap();
        assert_eq!(r.version, "0.2.0");
        assert_eq!(r.notes, "Notes");
    }

    #[test]
    fn same_or_older_release_is_not() {
        assert_eq!(pick(release("v0.1.0", "").as_bytes(), "0.1.0", false).unwrap(), None);
        assert_eq!(pick(release("v0.0.9", "").as_bytes(), "0.1.0", false).unwrap(), None);
    }

    #[test]
    fn force_offers_the_current_release_again() {
        assert!(pick(release("v0.1.0", "").as_bytes(), "0.1.0", true).unwrap().is_some());
        assert!(pick(release("v9.0.0", r#","draft":true"#).as_bytes(), "0.1.0", true).unwrap().is_none());
    }

    #[test]
    fn the_tool_is_optional_in_a_release() {
        let r = pick(release("v0.2.0", "").as_bytes(), "0.1.0", false).unwrap().unwrap();
        assert_eq!(r.url(Program::Cli), None);
        let with_cli = release("v0.2.0", "").replace(
            "]}",
            &format!(r#",{{"name":"{CLI_ASSET}","browser_download_url":"{DOWNLOAD_PREFIX}v0.2.0/{CLI_ASSET}"}}]}}"#),
        );
        let r = pick(with_cli.as_bytes(), "0.1.0", false).unwrap().unwrap();
        assert!(r.url(Program::Cli).is_some());
    }

    #[test]
    fn drafts_and_prereleases_are_skipped() {
        assert_eq!(pick(release("v9.0.0", r#","prerelease":true"#).as_bytes(), "0.1.0", false).unwrap(), None);
        assert_eq!(pick(release("v9.0.0", r#","draft":true"#).as_bytes(), "0.1.0", false).unwrap(), None);
    }

    #[test]
    fn foreign_links_are_refused() {
        let evil = release("v0.2.0", "").replace(DOWNLOAD_PREFIX, "https://example.com/");
        assert!(pick(evil.as_bytes(), "0.1.0", false).is_err());
        assert!(!link_ok(&format!("{DOWNLOAD_PREFIX}v1/../../x")));
        assert!(!link_ok("https://github.com/someone-else/meowtify/releases/download/v1/x"));
        assert!(link_ok(&format!("{DOWNLOAD_PREFIX}v1.0.0/{ASSET}")));
    }

    #[test]
    fn a_release_without_the_program_is_an_error() {
        let body = r#"{"tag_name":"v0.2.0","assets":[]}"#;
        assert!(pick(body.as_bytes(), "0.1.0", false).is_err());
    }

    #[test]
    fn a_304_reuses_the_stored_answer() {
        let body = release("v0.2.0", "").into_bytes();
        let cached = Cached { etag: "\"abc\"".into(), body: body.clone() };
        assert_eq!(reuse(Some(&cached)).unwrap(), body.as_slice());
        assert!(reuse(None).is_err(), "a 304 with nothing stored cannot be used");
        let r = pick(reuse(Some(&cached)).unwrap(), "0.1.0", false).unwrap().unwrap();
        assert_eq!(r.version, "0.2.0");
    }

    #[test]
    fn statuses_say_what_to_do() {
        assert_eq!(classify(200), Answer::Fresh);
        assert_eq!(classify(304), Answer::Unchanged);
        assert_eq!(classify(403), Answer::RateLimited);
        assert_eq!(classify(429), Answer::RateLimited);
        assert_eq!(classify(404), Answer::Failed);
        assert_eq!(classify(500), Answer::Failed);
    }

    #[test]
    fn headers_give_the_last_status_and_etag() {
        let dump = "HTTP/1.1 301 Moved Permanently\r\nLocation: https://api.github.com/x\r\n\r\n\
                    HTTP/1.1 304 Not Modified\r\nETag: \"v2\"\r\nX-RateLimit-Remaining: 0\r\n\r\n";
        assert_eq!(read_head(dump), (Some(304), Some("\"v2\"".into())));
        assert_eq!(read_head("HTTP/1.1 403 Forbidden\n"), (Some(403), None));
        assert_eq!(read_head(""), (None, None));
    }

    #[test]
    fn the_rate_limit_pauses_longer() {
        assert_eq!(wait_after(false), RECHECK_EVERY);
        assert_eq!(wait_after(true), RATE_LIMIT_PAUSE);
        assert!(RATE_LIMIT_PAUSE > RECHECK_EVERY);
        assert!(rate_limited(&ProviderError::Tool(RATE_LIMIT_TEXT.into())));
        assert!(!rate_limited(&ProviderError::Tool("something else".into())));
    }

    #[test]
    fn the_listed_size_is_kept_for_progress() {
        let sized = release("v0.2.0", "").replace(&format!(r#""name":"{ASSET}""#), &format!(r#""name":"{ASSET}","size":1234"#));
        let r = pick(sized.as_bytes(), "0.1.0", false).unwrap().unwrap();
        assert_eq!(r.size(Program::App), Some(1234));
        assert_eq!(r.size(Program::Cli), None);
    }

    #[test]
    fn side_files_stay_in_the_same_folder() {
        let p = side(Path::new(r"C:\apps\meowtify.exe"), "old");
        assert_eq!(p, Path::new(r"C:\apps\meowtify.exe.old"));
    }
}
