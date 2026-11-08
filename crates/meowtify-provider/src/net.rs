//! The proxy every network call goes through. One global setting, because yt-dlp, `curl`
//! and the streaming download are separate programs that all need the same address.

use std::sync::RwLock;

use tokio::process::Command;

use crate::{ProviderError, Result};

static PROXY: RwLock<Option<String>> = RwLock::new(None);

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

/// Adds `--proxy` for curl, and for yt-dlp (same flag).
pub(crate) fn apply(cmd: &mut Command) {
    if let Some(p) = proxy() {
        cmd.arg("--proxy").arg(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
