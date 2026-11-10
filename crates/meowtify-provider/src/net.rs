//! The proxy every network call goes through. One global setting, because yt-dlp, `curl`
//! and the streaming download are separate programs that all need the same address.

use std::path::PathBuf;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use tokio::process::Command;

use crate::{ProviderError, Result};

static PROXY: RwLock<Option<String>> = RwLock::new(None);

/// The browser yt-dlp may read cookies from when the user turns browser sign-in on.
static COOKIES: RwLock<Option<String>> = RwLock::new(None);

/// The cookie file the in-app YouTube sign-in wrote, decrypted for this run. It takes
/// priority over the browser: the user chose it more recently and it is a single account.
static LOGIN_COOKIES: RwLock<Option<PathBuf>> = RwLock::new(None);

/// The browsers yt-dlp can take cookies from. Only these names may reach its command line.
pub const COOKIE_BROWSERS: &[&str] = &["firefox", "chrome", "edge", "brave", "chromium", "opera", "vivaldi"];

const SCHEMES: [&str; 6] = ["http://", "https://", "socks4://", "socks4a://", "socks5://", "socks5h://"];

/// Checks a proxy address such as `http://127.0.0.1:8080` or `socks5h://user:pass@host:1080`.
/// Empty text means no proxy. The address goes to other programs as one argument, so it must
/// not hold spaces or control characters, and the scheme keeps it from reading as an option.
pub fn validate_proxy(text: &str) -> Result<Option<String>> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let ok_scheme = SCHEMES.iter().any(|s| t.len() > s.len() && t.is_char_boundary(s.len()) && t[..s.len()].eq_ignore_ascii_case(s));
    if !ok_scheme {
        return Err(ProviderError::Invalid(
            "The proxy must start with http://, https://, socks4://, socks5:// or socks5h://".into(),
        ));
    }
    if t.len() > 300 || t.chars().any(|c| c.is_control() || c.is_whitespace() || !c.is_ascii()) {
        return Err(ProviderError::Invalid("The proxy address has characters that are not allowed.".into()));
    }
    Ok(Some(t.to_string()))
}

/// Sets the proxy for all later network calls. Empty text clears it.
pub fn set_proxy(text: &str) -> Result<()> {
    let value = validate_proxy(text)?;
    *PROXY.write().unwrap() = value;
    Ok(())
}

pub fn proxy() -> Option<String> {
    PROXY.read().unwrap().clone()
}

/// Checks a browser name for `--cookies-from-browser`. Empty text turns the feature off.
/// The name goes on a command line, so only the exact lowercase names are accepted.
fn validate_cookies_browser(text: &str) -> std::result::Result<Option<&'static str>, String> {
    let name = text.trim();
    if name.is_empty() {
        return Ok(None);
    }
    COOKIE_BROWSERS
        .iter()
        .find(|b| **b == name)
        .copied()
        .map(Some)
        .ok_or_else(|| format!("{name} is not a browser cookies can be read from"))
}

/// Sets the browser yt-dlp reads cookies from. Empty text turns it off. Choosing a browser
/// again also ends the pause after a failure, so the user can try once more.
pub fn set_cookies_browser(text: &str) -> std::result::Result<(), String> {
    let value = validate_cookies_browser(text)?.map(str::to_string);
    *COOKIES.write().unwrap() = value;
    *COOKIES_FAILED.lock().unwrap_or_else(|e| e.into_inner()) = None;
    Ok(())
}

/// The browser the user chose, whether or not it works right now.
pub fn cookies_browser() -> Option<String> {
    COOKIES.read().unwrap().clone()
}

/// Sets (or clears) the cookie file yt-dlp reads with `--cookies`, written by the in-app
/// sign-in. Setting it also ends the pause after a refused sign-in, so a fresh sign-in is
/// tried at once.
pub fn set_login_cookies(path: Option<PathBuf>) {
    *LOGIN_COOKIES.write().unwrap_or_else(|e| e.into_inner()) = path;
    *LOGIN_FAILED.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// The cookie file of the in-app sign-in, whether or not it works right now. Its existence
/// is what the Settings row shows as signed in.
pub fn login_cookies() -> Option<PathBuf> {
    LOGIN_COOKIES.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// How long yt-dlp runs without cookies after the browser's cookies could not be read.
const COOKIES_PAUSE: Duration = Duration::from_secs(30 * 60);

/// When reading the browser's cookies last failed, and whether the user was told yet.
static COOKIES_FAILED: Mutex<Option<(Instant, bool)>> = Mutex::new(None);

/// The same for the sign-in's cookie file: when yt-dlp refused it, and whether the user
/// was told yet.
static LOGIN_FAILED: Mutex<Option<(Instant, bool)>> = Mutex::new(None);

/// The browser whose cookies yt-dlp should use now: the chosen one, unless reading its cookies
/// failed lately. A browser that cannot be read must not stop every download.
pub(crate) fn cookies_in_use() -> Option<String> {
    let browser = cookies_browser()?;
    let failed = COOKIES_FAILED.lock().unwrap_or_else(|e| e.into_inner()).map(|(t, _)| t);
    match failed {
        Some(t) if t.elapsed() < COOKIES_PAUSE => None,
        _ => Some(browser),
    }
}

/// The sign-in cookie file yt-dlp should use now, unless YouTube refused it lately. A stale
/// sign-in must not make every download fail: while it is paused, yt-dlp runs as a guest or
/// with the browser's cookies.
pub(crate) fn login_cookies_in_use() -> Option<PathBuf> {
    let path = login_cookies()?;
    let failed = LOGIN_FAILED.lock().unwrap_or_else(|e| e.into_inner()).map(|(t, _)| t);
    match failed {
        Some(t) if t.elapsed() < COOKIES_PAUSE => None,
        _ => Some(path),
    }
}

/// True when yt-dlp could not get the browser's cookies (the browser has the database open, or
/// its encryption cannot be read), as opposed to YouTube refusing the request.
pub(crate) fn is_cookie_error(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    t.contains("cookie database")
        || t.contains("cookies database")
        || t.contains("failed to decrypt")
        || (t.contains("could not find") && t.contains("cookies"))
        || t.contains("cookies-from-browser")
        || t.contains("unsupported browser")
}

/// Notes that the browser's cookies could not be read: yt-dlp runs without them for a while.
pub(crate) fn cookies_failed() {
    *COOKIES_FAILED.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), false));
}

/// True when yt-dlp says the sign-in's cookie file is not accepted: YouTube wants a signed-in
/// session, or the cookies themselves are stale. The file stays on disk; only its use pauses.
pub(crate) fn is_login_cookie_error(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    t.contains("sign in to confirm")
        || t.contains("sign in to view")
        || t.contains("login required")
        || t.contains("cookies are no longer valid")
        || t.contains("provided cookies")
        || (t.contains("cookie") && (t.contains("expired") || t.contains("invalid") || t.contains("failed")))
        || (t.contains("failed to decrypt") && t.contains("cookie"))
}

/// Notes that YouTube refused the sign-in's cookies: yt-dlp runs without them for a while,
/// so one stale sign-in does not fail every download in a loop.
pub(crate) fn login_cookies_failed() {
    *LOGIN_FAILED.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), false));
}

/// The sentence for the user after a failure, once per failure, or `None`. The in-app
/// sign-in is named first: it is the newer, more specific way to sign in.
pub fn take_cookie_problem() -> Option<String> {
    // Only when a sign-in is set: an old pause must not speak for one made later.
    if login_cookies().is_some() {
        let mut failed = LOGIN_FAILED.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((when, false)) = *failed {
            *failed = Some((when, true));
            return Some("YouTube sign-in expired. Sign in again in Settings.".to_string());
        }
    }
    let browser = cookies_browser()?;
    let mut failed = COOKIES_FAILED.lock().unwrap_or_else(|e| e.into_inner());
    let (when, told) = (*failed)?;
    if told {
        return None;
    }
    *failed = Some((when, true));
    let name = browser[..1].to_ascii_uppercase() + &browser[1..];
    let advice = if browser == "firefox" {
        "Check that you are signed in to YouTube in Firefox.".to_string()
    } else {
        format!("Close {name} and try again, or choose Firefox, which works best.")
    };
    Some(format!("Could not read the {name} sign-in, so songs download without it. {advice}"))
}

/// Adds `--proxy` for curl, and for yt-dlp (same flag).
pub(crate) fn apply(cmd: &mut Command) {
    if let Some(p) = proxy() {
        cmd.arg("--proxy").arg(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cookie settings are global, so tests that change them take turns.
    static GLOBALS: Mutex<()> = Mutex::new(());

    #[test]
    fn a_cookie_failure_is_told_from_youtube_refusing() {
        assert!(is_cookie_error("Could not copy Chrome cookie database. See  https://github.com/yt-dlp/yt-dlp/issues/7271  for more info"));
        assert!(is_cookie_error("ERROR: Failed to decrypt with DPAPI"));
        assert!(is_cookie_error("could not find firefox cookies database in C:\\x"));
        assert!(!is_cookie_error("Sign in to confirm you're not a bot"));
        assert!(!is_cookie_error("Video unavailable"));
    }

    #[test]
    fn accepts_the_usual_forms() {
        assert_eq!(validate_proxy("").unwrap(), None);
        assert_eq!(validate_proxy("  ").unwrap(), None);
        assert_eq!(validate_proxy(" http://127.0.0.1:8080 ").unwrap().as_deref(), Some("http://127.0.0.1:8080"));
        assert_eq!(validate_proxy("SOCKS5h://u:p@host:1080").unwrap().as_deref(), Some("SOCKS5h://u:p@host:1080"));
    }

    /// A dead proxy must stop curl calls, which proves the setting reaches the program.
    /// Port 9 (discard) refuses connections, so no network is needed.
    #[tokio::test]
    async fn curl_calls_go_through_the_proxy() {
        set_proxy("http://127.0.0.1:9").unwrap();
        let through_dead_proxy = crate::suggest("hello").await;
        set_proxy("").unwrap();
        assert!(through_dead_proxy.is_err(), "the request ignored the proxy: {through_dead_proxy:?}");
        assert_eq!(proxy(), None);
    }

    #[test]
    fn refuses_options_and_odd_text() {
        for bad in ["--output x", "127.0.0.1:8080", "ftp://h", "http://", "http://a b", "http://h\n--x", "http://h\u{e9}"] {
            assert!(validate_proxy(bad).is_err(), "{bad:?}");
        }
        assert!(validate_proxy(&format!("http://{}", "a".repeat(400))).is_err());
    }

    /// Validation and the command line are checked together so no other test can change the
    /// global setting between the two.
    #[test]
    fn browser_cookies_are_validated_and_reach_the_command() {
        let _g = GLOBALS.lock().unwrap();
        for good in COOKIE_BROWSERS {
            assert_eq!(set_cookies_browser(good).ok(), Some(()), "{good}");
        }
        assert_eq!(set_cookies_browser("").ok(), Some(()));
        assert_eq!(cookies_browser(), None);
        for bad in ["Firefox", "safari", "--proxy", "chrome firefox", "chromium.x", "edge;rm"] {
            assert!(set_cookies_browser(bad).is_err(), "{bad:?}");
        }
        assert_eq!(cookies_browser(), None, "a refused name is not kept");

        let yt = crate::youtube::YouTube::new(crate::youtube::Tools {
            yt_dlp: std::path::PathBuf::from("yt-dlp"),
            ffmpeg: None,
        });
        let args = |cmd: &tokio::process::Command| -> Vec<String> {
            cmd.as_std().get_args().map(|a| a.to_string_lossy().into_owned()).collect()
        };
        assert!(!args(&yt.command()).iter().any(|a| a == "--cookies-from-browser"), "no flag when off");
        set_cookies_browser("firefox").unwrap();
        let with_flag = args(&yt.command());
        let at = with_flag.iter().position(|a| a == "--cookies-from-browser").expect("the flag is passed");
        assert_eq!(with_flag.get(at + 1).map(String::as_str), Some("firefox"));
        set_cookies_browser("").unwrap();
        assert!(!args(&yt.command()).iter().any(|a| a == "--cookies-from-browser"), "off again");
    }

    #[test]
    fn the_sign_in_cookie_file_wins_and_a_refusal_pauses_it() {
        let _g = GLOBALS.lock().unwrap();
        let yt = crate::youtube::YouTube::new(crate::youtube::Tools {
            yt_dlp: std::path::PathBuf::from("yt-dlp"),
            ffmpeg: None,
        });
        let args = |cmd: &tokio::process::Command| -> Vec<String> {
            cmd.as_std().get_args().map(|a| a.to_string_lossy().into_owned()).collect()
        };
        let file = PathBuf::from("C:/meowtify/tmp/session-cookies.txt");

        // Both set: the sign-in file is the more specific choice.
        set_cookies_browser("firefox").unwrap();
        set_login_cookies(Some(file.clone()));
        let with_file = args(&yt.command());
        let at = with_file.iter().position(|a| a == "--cookies").expect("--cookies is passed");
        assert_eq!(with_file.get(at + 1).map(String::as_str), Some("C:/meowtify/tmp/session-cookies.txt"));
        assert!(!with_file.iter().any(|a| a == "--cookies-from-browser"));

        // YouTube refused it: it is no longer passed; the browser still is.
        login_cookies_failed();
        let paused = args(&yt.command());
        assert!(!paused.iter().any(|a| a == "--cookies"));
        assert!(paused.iter().any(|a| a == "--cookies-from-browser"));

        // Signing out removes it entirely.
        set_login_cookies(None);
        set_cookies_browser("").unwrap();
        let off = args(&yt.command());
        assert!(!off.iter().any(|a| a == "--cookies" || a == "--cookies-from-browser"));
    }

    #[test]
    fn an_expired_sign_in_is_told_once_and_only_when_one_is_set() {
        let _g = GLOBALS.lock().unwrap();
        set_cookies_browser("").unwrap();
        set_login_cookies(None);
        login_cookies_failed();
        assert_eq!(take_cookie_problem(), None, "no sign-in, nothing to say");

        set_login_cookies(Some(PathBuf::from("C:/x/cookies.txt")));
        login_cookies_failed();
        assert_eq!(
            take_cookie_problem().as_deref(),
            Some("YouTube sign-in expired. Sign in again in Settings.")
        );
        assert_eq!(take_cookie_problem(), None, "said once per failure");
        set_login_cookies(None);
    }

    #[test]
    fn a_stale_sign_in_is_recognised_in_yt_dlp_errors() {
        for text in [
            "Sign in to confirm you're not a bot. Use --cookies-from-browser or --cookies for the authentication",
            "ERROR: Sign in to view this video",
            "Cookies are no longer valid, or the account is restricted",
            "The provided cookies are invalid",
            "Login required",
        ] {
            assert!(is_login_cookie_error(text), "{text:?}");
        }
        assert!(!is_login_cookie_error("Video unavailable"));
        assert!(!is_login_cookie_error("Could not copy Chrome cookie database"));
    }
}
