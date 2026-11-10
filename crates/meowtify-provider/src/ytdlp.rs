//! The yt-dlp program: where it is, and keeping it current without the user
//! doing anything by hand.
//!
//! YouTube breaks old yt-dlp versions within weeks. Copies installed by pip or
//! a package manager cannot update themselves, so the app keeps its own
//! standalone copy in the tools folder. Updating compares the SHA-256 of that
//! copy with the one in the latest release's `SHA2-256SUMS`, downloads only on
//! a difference, checks the new file, and swaps it in. Downloads go through
//! `curl`, which Windows 10 and later ship, so the app carries no HTTP stack.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::RwLock;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::process::Command;

use crate::youtube::{plain_command, yt_dlp_version};
use crate::{ProviderError, Result};

const BASE_URL: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/";
const ASSET: &str = if cfg!(windows) { "yt-dlp.exe" } else { "yt-dlp" };
/// Windows: the folder build of yt-dlp. The single-file exe unpacks itself on every
/// run, which costs about 0.6 s before any work starts. The folder build does not.
#[cfg(windows)]
const DIR_ASSET: &str = "yt-dlp_win.zip";
/// Folder inside the tools folder that holds the unpacked folder build.
pub(crate) const DIR_NAME: &str = "yt-dlp-win";

/// What an update did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateOutcome {
    pub version: String,
    /// `false` when the installed copy already matched the latest release.
    pub changed: bool,
}

/// The yt-dlp path in use. It changes when an update installs the app's own copy.
pub struct YtDlp {
    tools_dir: PathBuf,
    path: RwLock<PathBuf>,
}

impl YtDlp {
    pub fn new(path: PathBuf, tools_dir: PathBuf) -> YtDlp {
        YtDlp { tools_dir, path: RwLock::new(path) }
    }

    pub fn path(&self) -> PathBuf {
        self.path.read().map(|p| p.clone()).unwrap_or_default()
    }

    pub async fn version(&self) -> Option<String> {
        yt_dlp_version(&self.path()).await
    }

    /// Makes sure the app's own copy is the latest release.
    pub async fn update(&self) -> Result<UpdateOutcome> {
        let target = self.tools_dir.join(ASSET);
        let sums = curl_text(&format!("{BASE_URL}SHA2-256SUMS")).await?;
        // The folder build starts faster. If it cannot be set up, the single file still works.
        #[cfg(windows)]
        if let Some(zip_hash) = expected_hash(&sums, DIR_ASSET) {
            if let Ok(outcome) = self.update_dir(&zip_hash).await {
                return Ok(outcome);
            }
        }
        let expected = expected_hash(&sums, ASSET).ok_or_else(|| ProviderError::Parse("no yt-dlp entry in SHA2-256SUMS".into()))?;

        if target.is_file() && hash_file(&target).await.as_deref() == Some(expected.as_str()) {
            self.set(target.clone());
            let version = yt_dlp_version(&target).await.unwrap_or_else(|| "unknown".to_string());
            return Ok(UpdateOutcome { version, changed: false });
        }

        std::fs::create_dir_all(&self.tools_dir).map_err(|e| ProviderError::Io(e.to_string()))?;
        let part = self.tools_dir.join(format!("{ASSET}.new"));
        let result = self.install(&target, &part, &expected).await;
        let _ = std::fs::remove_file(&part);
        result
    }

    /// Keeps the folder build in `tools/yt-dlp-win` current. A marker file next to
    /// it holds the checksum of the zip it came from, so a check costs no download.
    #[cfg(windows)]
    async fn update_dir(&self, expected: &str) -> Result<UpdateOutcome> {
        let dir = self.tools_dir.join(DIR_NAME);
        let exe = dir.join(ASSET);
        let marker = dir.join(".sha256");
        if exe.is_file() && std::fs::read_to_string(&marker).ok().as_deref().map(str::trim) == Some(expected) {
            self.set(exe.clone());
            let version = yt_dlp_version(&exe).await.unwrap_or_else(|| "unknown".to_string());
            return Ok(UpdateOutcome { version, changed: false });
        }
        std::fs::create_dir_all(&self.tools_dir).map_err(|e| ProviderError::Io(e.to_string()))?;
        let zip = self.tools_dir.join(format!("{DIR_ASSET}.new"));
        let stage = self.tools_dir.join(format!("{DIR_NAME}.new"));
        let result = self.install_dir(&zip, &stage, &dir, expected).await;
        let _ = std::fs::remove_file(&zip);
        let _ = std::fs::remove_dir_all(&stage);
        result
    }

    #[cfg(windows)]
    async fn install_dir(&self, zip: &Path, stage: &Path, dir: &Path, expected: &str) -> Result<UpdateOutcome> {
        curl_file(&format!("{BASE_URL}{DIR_ASSET}"), zip).await?;
        if hash_file(zip).await.as_deref() != Some(expected) {
            return Err(ProviderError::Tool("the download did not match its checksum".to_string()));
        }
        let _ = std::fs::remove_dir_all(stage);
        std::fs::create_dir_all(stage).map_err(|e| ProviderError::Io(e.to_string()))?;
        // bsdtar ships with Windows 10 and later and reads zip files. Use the system copy:
        // a GNU tar earlier on PATH cannot.
        let tar = std::env::var_os("SystemRoot")
            .map(|r| PathBuf::from(r).join("System32").join("tar.exe"))
            .unwrap_or_else(|| PathBuf::from("tar.exe"));
        let mut c = plain_command(&tar);
        c.arg("-xf").arg(zip).arg("-C").arg(stage).stdout(Stdio::null()).stderr(Stdio::piped());
        let out = tokio::time::timeout(Duration::from_secs(120), c.output())
            .await
            .map_err(|_| ProviderError::Tool("unpacking yt-dlp timed out".to_string()))?
            .map_err(|e| ProviderError::Io(e.to_string()))?;
        if !out.status.success() {
            return Err(ProviderError::Tool(format!("could not unpack yt-dlp: {}", String::from_utf8_lossy(&out.stderr).trim())));
        }
        let version = yt_dlp_version(&stage.join(ASSET))
            .await
            .ok_or_else(|| ProviderError::Tool("the unpacked yt-dlp does not run".to_string()))?;
        std::fs::write(stage.join(".sha256"), expected).map_err(|e| ProviderError::Io(e.to_string()))?;

        // A folder with a running exe cannot be renamed on Windows; the error is returned
        // and the single-file path takes over until the next launch.
        let old = self.tools_dir.join(format!("{DIR_NAME}.old"));
        let _ = std::fs::remove_dir_all(&old);
        if dir.exists() {
            std::fs::rename(dir, &old).map_err(|e| ProviderError::Io(e.to_string()))?;
        }
        if let Err(e) = std::fs::rename(stage, dir) {
            let _ = std::fs::rename(&old, dir);
            return Err(ProviderError::Io(e.to_string()));
        }
        let _ = std::fs::remove_dir_all(&old);
        self.set(dir.join(ASSET));
        Ok(UpdateOutcome { version, changed: true })
    }

    async fn install(&self, target: &Path, part: &Path, expected: &str) -> Result<UpdateOutcome> {
        curl_file(&format!("{BASE_URL}{ASSET}"), part).await?;
        if hash_file(part).await.as_deref() != Some(expected) {
            return Err(ProviderError::Tool("the download did not match its checksum".to_string()));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(part, std::fs::Permissions::from_mode(0o755));
        }
        let version = yt_dlp_version(part)
            .await
            .ok_or_else(|| ProviderError::Tool("the downloaded yt-dlp does not run".to_string()))?;

        // A running exe cannot be overwritten on Windows, but it can be renamed.
        let old = self.tools_dir.join(format!("{ASSET}.old"));
        let _ = std::fs::remove_file(&old);
        if target.is_file() {
            std::fs::rename(target, &old).map_err(|e| ProviderError::Io(e.to_string()))?;
        }
        if let Err(e) = std::fs::rename(part, target) {
            let _ = std::fs::rename(&old, target);
            return Err(ProviderError::Io(e.to_string()));
        }
        let _ = std::fs::remove_file(&old);
        self.set(target.to_path_buf());
        Ok(UpdateOutcome { version, changed: true })
    }

    fn set(&self, p: PathBuf) {
        if let Ok(mut w) = self.path.write() {
            *w = p;
        }
    }
}

fn curl() -> Command {
    let mut c = plain_command(Path::new(if cfg!(windows) { "curl.exe" } else { "curl" }));
    c.args(["--fail", "--location", "--silent", "--show-error", "--max-time", "300"]);
    crate::net::apply(&mut c);
    c
}

async fn curl_run(mut c: Command) -> Result<Vec<u8>> {
    c.stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = match tokio::time::timeout(Duration::from_secs(920), c.output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ProviderError::Tool("curl was not found, so yt-dlp cannot be downloaded".to_string()))
        }
        Ok(Err(e)) => return Err(ProviderError::Io(e.to_string())),
        Err(_) => return Err(ProviderError::Tool("the download timed out".to_string())),
    };
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(ProviderError::Tool(if msg.is_empty() { "the download failed".to_string() } else { msg }));
    }
    Ok(out.stdout)
}

pub(crate) async fn curl_text(url: &str) -> Result<String> {
    let mut c = curl();
    c.arg(url);
    Ok(String::from_utf8_lossy(&curl_run(c).await?).into_owned())
}

pub(crate) async fn curl_file(url: &str, dest: &Path) -> Result<()> {
    let mut c = curl();
    c.args(["--max-time", "900"]).arg("-o").arg(dest).arg(url);
    curl_run(c).await.map(|_| ())
}

/// GETs `url` as a client that names itself, with a short time limit. For small API answers.
pub(crate) async fn curl_get(url: &str, user_agent: &str, max_secs: u32) -> Result<Vec<u8>> {
    let mut c = curl();
    c.args(["--max-time", &max_secs.to_string(), "--compressed", "--user-agent", user_agent, url]);
    curl_run(c).await
}

const SUGGEST_URL: &str = "https://suggestqueries.google.com/complete/search?client=firefox&ds=yt&hl=en&q=";

/// Search suggestions for what the user has typed so far, best first. Short
/// (4 seconds at most) and read-only; an empty list means none.
pub async fn suggest(query: &str) -> Result<Vec<String>> {
    let q = query.trim();
    if q.is_empty() || q.chars().count() > 100 || q.chars().any(char::is_control) {
        return Ok(Vec::new());
    }
    // In process: the search box also works where curl cannot run (iOS).
    let url = format!("{SUGGEST_URL}{}", crate::youtube::url_encode(q));
    let answer = tokio::time::timeout(
        std::time::Duration::from_secs(4),
        crate::native::http::get_json(&url, &[("User-Agent", crate::native::http::DESKTOP_UA)], 256 * 1024, "Google"),
    )
    .await
    .map_err(|_| ProviderError::Tool("the suggestion service took too long".into()))??;
    parse_suggestions_value(&answer)
}

/// The answer is `["typed", ["suggestion", ...], ...]`.
#[cfg(test)]
fn parse_suggestions(body: &[u8]) -> Result<Vec<String>> {
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| ProviderError::Parse(e.to_string()))?;
    parse_suggestions_value(&v)
}

fn parse_suggestions_value(v: &serde_json::Value) -> Result<Vec<String>> {
    Ok(v.get(1)
        .and_then(|a| a.as_array())
        .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
        .unwrap_or_default())
}

const THUMB_PREFIX: &str = "https://i.ytimg.com/vi/";

/// Whether `url` is a cover link of a fallback source (SoundCloud, Audius): https, a named host
/// (no IP address, no `localhost`, no login part), no spaces or control characters. The content
/// nodes of Audius change, so the host cannot be a fixed list; the 1 MB limit and the image
/// decoder bound what a wrong link can do.
pub fn external_cover_url_ok(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else { return false };
    if url.len() > 400 || url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') || authority.contains('[') {
        return false;
    }
    let host = authority.split(':').next().unwrap_or("").to_ascii_lowercase();
    let named = host.contains('.') && host.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
    let numeric = host.chars().all(|c| c.is_ascii_digit() || c == '.');
    named && !numeric && host != "localhost" && !host.ends_with(".local") && !host.ends_with(".internal")
}

/// Downloads a cover image (at most 1 MB, 10 seconds). Accepts `i.ytimg.com` video covers and
/// the cover links of the fallback sources (see [`external_cover_url_ok`]), so this cannot be
/// pointed at a local address.
pub async fn fetch_thumbnail(url: &str) -> Result<Vec<u8>> {
    let yt = url.len() <= 200
        && url.starts_with(THUMB_PREFIX)
        && url[THUMB_PREFIX.len()..].chars().all(|c| c.is_ascii_alphanumeric() || "-_/.".contains(c));
    if !yt && !external_cover_url_ok(url) {
        return Err(ProviderError::Invalid("not a cover link".to_string()));
    }
    let mut reply = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        crate::native::http::get(url, &[("User-Agent", crate::native::http::DESKTOP_UA)]),
    )
    .await
    .map_err(|_| ProviderError::Tool("the cover took too long".into()))??;
    if reply.status != 200 {
        return Err(ProviderError::Tool(format!("the cover server answered {}", reply.status)));
    }
    reply.bytes(1_000_000).await
}

/// The hash listed for the yt-dlp program in a `SHA2-256SUMS` file.
pub(crate) fn expected_hash(sums: &str, asset: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        let (hash, name) = (it.next()?, it.next()?);
        (name.trim_start_matches('*') == asset).then(|| hash.to_ascii_lowercase())
    })
}

/// SHA-256 as lowercase hex. Reads in small blocks so a large file costs no memory.
pub(crate) async fn hash_file(path: &Path) -> Option<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut f = std::fs::File::open(path).ok()?;
        let mut h = Sha256::new();
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = f.read(&mut buf).ok()?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
        Some(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
    })
    .await
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_covers_must_be_named_https_hosts() {
        assert!(external_cover_url_ok("https://i1.sndcdn.com/artworks-abc-large.jpg"));
        assert!(external_cover_url_ok("https://creatornode.audius.co/content/x/480x480.jpg"));
        assert!(!external_cover_url_ok("http://i1.sndcdn.com/a.jpg"));
        assert!(!external_cover_url_ok("https://127.0.0.1/a.jpg"));
        assert!(!external_cover_url_ok("https://192.168.1.2:8080/a.jpg"));
        assert!(!external_cover_url_ok("https://localhost/a.jpg"));
        assert!(!external_cover_url_ok("https://user@evil.example/a.jpg"));
        assert!(!external_cover_url_ok("https://[::1]/a.jpg"));
        assert!(!external_cover_url_ok("https://printer.local/a.jpg"));
        assert!(!external_cover_url_ok("https://a.example/a b.jpg"));
    }

    #[test]
    fn reads_the_suggestion_list() {
        let body = br#"["tsu",["tsubi club","tsunami"],[],{"google:suggestsubtypes":[[512],[512]]}]"#;
        assert_eq!(parse_suggestions(body).unwrap(), vec!["tsubi club", "tsunami"]);
        assert_eq!(parse_suggestions(b"[\"x\"]").unwrap(), Vec::<String>::new());
        assert!(parse_suggestions(b"nope").is_err());
    }

    #[test]
    fn finds_the_hash_for_the_program() {
        let sums = format!("AAA1  yt-dlp_x86.exe\nBBB2  {ASSET}\nCCC3 *yt-dlp_linux\n");
        assert_eq!(expected_hash(&sums, ASSET).as_deref(), Some("bbb2"));
        assert_eq!(expected_hash("AAA1  other\n", ASSET), None);
        assert_eq!(expected_hash("DDD4  yt-dlp_win.zip\n", "yt-dlp_win.zip").as_deref(), Some("ddd4"));
    }

    /// Needs the network: `cargo test -p meowtify-provider live_thumbnail -- --ignored --nocapture`.
    /// The cover of every song the list has not cached comes through this call.
    #[tokio::test]
    #[ignore]
    async fn live_thumbnail_is_fetched_by_the_in_process_client() {
        for id in ["dQw4w9WgXcQ", "qg18wn-GDRM", "RmYCOm4ehKs"] {
            let started = std::time::Instant::now();
            let url = format!("{THUMB_PREFIX}{id}/mqdefault.jpg");
            let got = fetch_thumbnail(&url).await;
            eprintln!("{id}: {:?} in {:?}", got.as_ref().map(|b| b.len()), started.elapsed());
            assert!(got.is_ok_and(|b| b.len() > 1000), "no cover for {id}");
        }
    }

    /// Needs the network: `cargo test -p meowtify-provider live_update -- --ignored --nocapture`
    #[cfg(windows)]
    #[tokio::test]
    #[ignore]
    async fn live_update_installs_the_folder_build() {
        let dir = std::env::temp_dir().join(format!("meowtify-ytdlp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let y = YtDlp::new(dir.join("yt-dlp.exe"), dir.clone());
        let first = y.update().await.unwrap();
        assert!(first.changed);
        assert!(y.path().starts_with(dir.join(DIR_NAME)), "{:?}", y.path());
        let again = y.update().await.unwrap();
        assert!(!again.changed, "second check must not download");
        eprintln!("yt-dlp {} at {}", again.version, y.path().display());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
