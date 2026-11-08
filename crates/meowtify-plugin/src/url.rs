//! Strict URL parsing for the plugin fetch path.
//!
//! Only what the allowlist check needs: scheme, host, port and path. Anything
//! unusual is rejected instead of normalized, so the host that is checked is
//! the host that is contacted.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub https: bool,
    pub host: String,
    pub port: Option<u16>,
    /// Path plus query, always starting with '/'.
    pub path_query: String,
}

const MAX_URL: usize = 2048;

impl Url {
    pub fn parse(s: &str) -> Result<Url, String> {
        if s.len() > MAX_URL {
            return Err("url too long".into());
        }
        check_chars(s)?;
        let (https, rest) = if let Some(r) = strip_prefix_ci(s, "https://") {
            (true, r)
        } else if let Some(r) = strip_prefix_ci(s, "http://") {
            (false, r)
        } else {
            return Err("only http and https urls are allowed".into());
        };
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(end);
        if authority.contains('@') {
            return Err("userinfo is not allowed".into());
        }
        if authority.contains('[') || authority.contains(']') {
            return Err("ipv6 hosts are not allowed".into());
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => {
                let port: u16 = p.parse().map_err(|_| "bad port".to_string())?;
                if port == 0 {
                    return Err("bad port".into());
                }
                (h, Some(port))
            }
            None => (authority, None),
        };
        let host = host.to_ascii_lowercase();
        if !valid_hostname(&host) {
            return Err("bad hostname".into());
        }
        let tail = tail.split('#').next().unwrap_or("");
        let path_query = if tail.is_empty() {
            "/".to_string()
        } else if tail.starts_with('?') {
            format!("/{tail}")
        } else {
            tail.to_string()
        };
        Ok(Url { https, host, port, path_query })
    }

    pub fn render(&self) -> String {
        let scheme = if self.https { "https" } else { "http" };
        match self.port {
            Some(p) => format!("{scheme}://{}:{p}{}", self.host, self.path_query),
            None => format!("{scheme}://{}{}", self.host, self.path_query),
        }
    }

    /// Resolves a redirect `Location` value against this url.
    pub fn resolve(&self, location: &str) -> Result<Url, String> {
        if location.len() > MAX_URL {
            return Err("redirect location too long".into());
        }
        check_chars(location)?;
        if location.contains("://") {
            return Url::parse(location);
        }
        if location.starts_with("//") {
            let scheme = if self.https { "https:" } else { "http:" };
            return Url::parse(&format!("{scheme}{location}"));
        }
        let mut out = self.clone();
        let location = location.split('#').next().unwrap_or("");
        if location.starts_with('/') {
            out.path_query = location.to_string();
        } else if location.starts_with('?') {
            let base = self.path_query.split('?').next().unwrap_or("/");
            out.path_query = format!("{base}{location}");
        } else if location.is_empty() {
            // Same document.
        } else {
            let base = self.path_query.split('?').next().unwrap_or("/");
            let dir = &base[..base.rfind('/').map(|i| i + 1).unwrap_or(1)];
            out.path_query = format!("{dir}{location}");
        }
        Ok(out)
    }
}

fn check_chars(s: &str) -> Result<(), String> {
    if !s.is_ascii() {
        return Err("non-ascii url".into());
    }
    if s.bytes().any(|b| b <= 0x20 || b == 0x7f || b == b'\\') {
        return Err("illegal character in url".into());
    }
    Ok(())
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len() && s.is_char_boundary(prefix.len()) && s[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

pub fn valid_hostname(h: &str) -> bool {
    if h.is_empty() || h.len() > 253 {
        return false;
    }
    h.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

/// Validates and lowercases an allowlist entry: `example.com` or `*.example.com`.
pub fn normalize_host_pattern(p: &str) -> Result<String, String> {
    let p = p.trim().to_ascii_lowercase();
    let base = p.strip_prefix("*.").unwrap_or(&p);
    if !valid_hostname(base) {
        return Err(format!("bad host pattern: {p}"));
    }
    Ok(p)
}

/// A wildcard pattern matches subdomains only, never the bare domain.
pub fn host_allowed(patterns: &[String], host: &str) -> bool {
    patterns.iter().any(|p| match p.strip_prefix("*.") {
        Some(base) => host.len() > base.len() + 1 && host.ends_with(base) && host.as_bytes()[host.len() - base.len() - 1] == b'.',
        None => p == host,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic() {
        let u = Url::parse("https://Example.com:8443/a/b?x=1#frag").unwrap();
        assert_eq!(u.host, "example.com");
        assert_eq!(u.port, Some(8443));
        assert_eq!(u.path_query, "/a/b?x=1");
        assert!(u.https);
        assert_eq!(u.render(), "https://example.com:8443/a/b?x=1");
    }

    #[test]
    fn rejects_tricks() {
        for bad in [
            "https://evil.com@good.com/",
            "https://good.com\\@evil.com/",
            "ftp://a.com/",
            "https://[::1]/",
            "https://a.com:0/",
            "https://exa mple.com/",
            "https://exämple.com/",
            "https://-a.com/",
            "https:///path",
        ] {
            assert!(Url::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn resolves_redirects() {
        let u = Url::parse("https://a.com/x/y?q=1").unwrap();
        assert_eq!(u.resolve("/z").unwrap().render(), "https://a.com/z");
        assert_eq!(u.resolve("w").unwrap().render(), "https://a.com/x/w");
        assert_eq!(u.resolve("//b.com/p").unwrap().render(), "https://b.com/p");
        assert_eq!(u.resolve("http://c.com").unwrap().render(), "http://c.com/");
    }

    #[test]
    fn host_matching() {
        let pats = vec!["api.example.com".to_string(), "*.cdn.net".to_string()];
        assert!(host_allowed(&pats, "api.example.com"));
        assert!(!host_allowed(&pats, "example.com"));
        assert!(host_allowed(&pats, "a.cdn.net"));
        assert!(!host_allowed(&pats, "cdn.net"));
        assert!(!host_allowed(&pats, "evilcdn.net"));
        assert!(!host_allowed(&pats, "api.example.com.evil.com"));
    }
}
