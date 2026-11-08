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

/// Asks the relay for ICE servers, with TURN credentials when it has them. Any failure gives
/// the STUN-only default, so a relay without `/turn` (such as the native one) still works.
///
/// Uses the system's `curl`, like the rest of the app's downloads, to avoid an HTTP client
/// that would be used once per jam.
pub async fn fetch(relay: &str, hide_ip: bool) -> IceConfig {
    let ice = fetch_servers(relay).await;
    if hide_ip { ice.hiding_ip() } else { ice }
}

async fn fetch_servers(relay: &str) -> IceConfig {
    let Some(url) = turn_url(relay) else { return IceConfig::default() };
    let mut cmd = tokio::process::Command::new("curl");
    cmd.args(["--silent", "--fail", "--max-time", "5", "--max-filesize", "65536", &url]).kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let out = tokio::time::timeout(std::time::Duration::from_secs(8), cmd.output()).await;
    match out {
        Ok(Ok(o)) if o.status.success() => String::from_utf8(o.stdout).ok().and_then(|t| IceConfig::from_json(&t)).unwrap_or_default(),
        _ => IceConfig::default(),
    }
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
