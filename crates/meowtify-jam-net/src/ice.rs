//! ICE servers: the STUN and TURN servers a peer to peer connection uses to find a path.

use serde::Deserialize;
use webrtc::peer_connection::RTCIceServer;

/// Used when the relay does not say otherwise.
pub const DEFAULT_STUN: &str = "stun:stun.cloudflare.com:3478";

#[derive(Debug, Clone, PartialEq)]
pub struct IceConfig {
    pub servers: Vec<RTCIceServer>,
    /// Use only TURN, so the other side sees the TURN server's address and not ours.
    pub relay_only: bool,
}

impl Default for IceConfig {
    /// STUN only. It connects most pairs; TURN covers the rest.
    fn default() -> IceConfig {
        IceConfig { servers: vec![RTCIceServer { urls: vec![DEFAULT_STUN.into()], ..Default::default() }], relay_only: false }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Urls {
    One(String),
    Many(Vec<String>),
}

#[derive(Deserialize)]
struct Server {
    urls: Urls,
    #[serde(default)]
    username: String,
    #[serde(default)]
    credential: String,
}

#[derive(Deserialize)]
struct Answer {
    #[serde(rename = "iceServers")]
    ice_servers: Vec<Server>,
}

impl IceConfig {
    /// Reads the answer of the relay's `/turn` (the format of the WebRTC `iceServers` list,
    /// which is also what Cloudflare Realtime TURN returns). Anything unusable gives `None`,
    /// and the caller falls back to [`IceConfig::default`].
    pub fn from_json(text: &str) -> Option<IceConfig> {
        let answer: Answer = serde_json::from_str(text).ok()?;
        let servers: Vec<RTCIceServer> = answer
            .ice_servers
            .into_iter()
            .map(|s| RTCIceServer {
                urls: match s.urls {
                    Urls::One(u) => vec![u],
                    Urls::Many(u) => u,
                },
                username: s.username,
                credential: s.credential,
            })
            .filter(|s| !s.urls.is_empty())
            .collect();
        (!servers.is_empty()).then_some(IceConfig { servers, relay_only: false })
    }
}

impl IceConfig {
    /// Asks for connections through TURN only, which hides our address from the other side.
    pub fn hiding_ip(mut self) -> IceConfig {
        self.relay_only = true;
        self
    }

    /// Whether the servers can do what the config asks. TURN-only needs a TURN server, which is
    /// one that comes with credentials.
    pub fn usable(&self) -> bool {
        !self.relay_only || self.servers.iter().any(|s| !s.username.is_empty())
    }
}

/// The HTTP address of a relay's `/turn`, from the relay's WebSocket address.
fn turn_url(relay: &str) -> Option<String> {
    let rest = relay.strip_prefix("wss://").map(|r| ("https://", r)).or_else(|| relay.strip_prefix("ws://").map(|r| ("http://", r)))?;
    Some(format!("{}{}/turn", rest.0, rest.1.trim_end_matches('/')))
}

/// Asks the relay for ICE servers, with TURN credentials when it has them.
///
/// A relay that answers without TURN (the 404 of a relay that has none) gives the STUN-only
/// default. A request that fails gives the same default, unless `hide_ip` is set: then the
/// caller must not fall back to a direct path, so it gets the reason as `Err`.
///
/// Uses the system's `curl.exe`, like the rest of the app's downloads, to avoid an HTTP client
/// that would be used once per jam. `proxy` is the app's proxy setting, if any.
pub async fn fetch(relay: &str, hide_ip: bool, proxy: Option<&str>) -> Result<IceConfig, String> {
    match fetch_servers(relay, proxy).await {
        Ok(ice) => Ok(if hide_ip { ice.hiding_ip() } else { ice }),
        Err(why) if hide_ip => Err(why),
        Err(_) => Ok(IceConfig::default()),
    }
}

enum Asked {
    /// The relay answered, but has no TURN.
    NoTurn,
    Failed(String),
}

async fn fetch_servers(relay: &str, proxy: Option<&str>) -> Result<IceConfig, String> {
    let Some(url) = turn_url(relay) else { return Ok(IceConfig::default()) };
    let mut why = String::new();
    for attempt in 0..2 {
        match ask(&url, proxy).await {
            Ok(text) => return Ok(IceConfig::from_json(&text).unwrap_or_default()),
            Err(Asked::NoTurn) => return Ok(IceConfig::default()),
            Err(Asked::Failed(w)) => why = w,
        }
        if attempt == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        }
    }
    Err(why)
}

async fn ask(url: &str, proxy: Option<&str>) -> Result<String, Asked> {
    let mut cmd = tokio::process::Command::new(if cfg!(windows) { "curl.exe" } else { "curl" });
    cmd.args(["--silent", "--show-error", "--fail", "--connect-timeout", "8", "--max-time", "12", "--max-filesize", "65536"]);
    if let Some(p) = proxy {
        cmd.arg("--proxy").arg(p);
    }
    cmd.arg(url).stdin(std::process::Stdio::null()).kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let out = match tokio::time::timeout(std::time::Duration::from_secs(16), cmd.output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => return Err(Asked::Failed("curl was not found on this computer".into())),
        Ok(Err(e)) => return Err(Asked::Failed(e.to_string())),
        Err(_) => return Err(Asked::Failed("the request timed out".into())),
    };
    if out.status.success() {
        return String::from_utf8(out.stdout).map_err(|_| Asked::Failed("the answer was not text".into()));
    }
    let said = String::from_utf8_lossy(&out.stderr);
    let said = said.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    if said.contains("returned error: 404") {
        return Err(Asked::NoTurn);
    }
    Err(Asked::Failed(if said.is_empty() { "the request failed".into() } else { said.chars().take(160).collect() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_what_cloudflare_returns() {
        let text = r#"{"iceServers":[
            {"urls":["stun:stun.cloudflare.com:3478","turn:turn.cloudflare.com:3478?transport=udp"],"username":"u","credential":"c"},
            {"urls":"stun:other:3478"}]}"#;
        let ice = IceConfig::from_json(text).unwrap();
        assert_eq!(ice.servers.len(), 2);
        assert_eq!((ice.servers[0].urls.len(), ice.servers[0].username.as_str()), (2, "u"));
        assert_eq!(ice.servers[1].urls, vec!["stun:other:3478".to_string()]);
    }

    #[test]
    fn the_turn_address_follows_the_relay_address() {
        assert_eq!(turn_url("wss://jam.example.com").as_deref(), Some("https://jam.example.com/turn"));
        assert_eq!(turn_url("ws://127.0.0.1:8787/").as_deref(), Some("http://127.0.0.1:8787/turn"));
        assert_eq!(turn_url("http://nope"), None);
    }

    #[test]
    fn unusable_answers_fall_back() {
        assert!(IceConfig::from_json("nonsense").is_none());
        assert!(IceConfig::from_json(r#"{"iceServers":[]}"#).is_none());
        assert_eq!(IceConfig::default().servers[0].urls, vec![DEFAULT_STUN.to_string()]);
    }

    #[test]
    fn hiding_the_ip_needs_a_turn_server() {
        assert!(IceConfig::default().usable());
        assert!(!IceConfig::default().hiding_ip().usable(), "STUN alone cannot hide the address");
        let turn = IceConfig::from_json(r#"{"iceServers":[{"urls":"turn:h:3478","username":"u","credential":"c"}]}"#).unwrap();
        assert!(turn.hiding_ip().usable());
    }
}
