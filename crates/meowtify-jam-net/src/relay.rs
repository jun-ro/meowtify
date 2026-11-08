//! The client side of the signaling relay: one WebSocket that reconnects by itself.
//!
//! After a drop, a guest says `hello` again with the token of its first `welcome`, and the relay
//! gives it its place back. The host does the same with its host key. A repeated `Welcome` in
//! the event stream therefore means "you are back; here is the room as it is now".

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use meowtify_jam::{ClientMsg, ErrorCode, LeftReason, MemberId, Resume, ServerMsg};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// Keeps idle connections open through proxies, and tells the relay we are alive.
const KEEPALIVE: Duration = Duration::from_secs(25);
/// Failed attempts in a row before the relay is given up on. The waits add up to about a minute.
const MAX_FAILURES: u32 = 8;
/// A connection that ends sooner than this did not work, even though it opened.
const STABLE_AFTER: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct RelayConfig {
    /// The relay, such as `wss://jam.example.workers.dev` or `ws://127.0.0.1:8787`.
    pub url: String,
    pub code: String,
    pub name: String,
    /// The avatar colour in degrees, or `None` to let peers derive it from the name.
    pub hue: Option<u16>,
    /// Set for the host: creates the room, and takes the host's place back after a drop.
    pub host_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RelayEvent {
    Msg(ServerMsg),
    /// The connection dropped, with the reason; the client is trying again.
    Reconnecting(String),
    /// Over for good: the room is gone, the host removed us, or the relay cannot be reached.
    Down(String),
}

pub struct Relay {
    tx: mpsc::UnboundedSender<ClientMsg>,
    task: tokio::task::AbortHandle,
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Relay {
    /// Connects in the background. Everything that happens comes out of the returned receiver.
    pub fn connect(cfg: RelayConfig) -> (Relay, mpsc::UnboundedReceiver<RelayEvent>) {
        let (tx, cmds) = mpsc::unbounded_channel();
        let (events_tx, events) = mpsc::unbounded_channel();
        let task = tokio::spawn(run(cfg, cmds, events_tx)).abort_handle();
        (Relay { tx, task }, events)
    }

    pub fn send(&self, msg: ClientMsg) {
        let _ = self.tx.send(msg);
    }
}

/// How long to wait after `failures` failed attempts in a row.
fn backoff(failures: u32) -> Duration {
    Duration::from_millis(500u64 << failures.min(4)) // 0.5, 1, 2, 4, 8 s
}

enum Ended {
    /// The connection dropped. Try again.
    Dropped,
    /// Do not try again.
    Over(String),
}

async fn run(cfg: RelayConfig, mut cmds: mpsc::UnboundedReceiver<ClientMsg>, events: mpsc::UnboundedSender<RelayEvent>) {
    let url = format!("{}/jam/{}", cfg.url.trim_end_matches('/'), cfg.code);
    let mut resume: Option<Resume> = None;
    let mut failures = 0u32;
    loop {
        let mut stable = false;
        let why;
        match tokio_tungstenite::connect_async(&url).await {
            Ok((ws, _)) => {
                let opened = std::time::Instant::now();
                match session(&cfg, ws, &mut cmds, &events, &mut resume).await {
                    Ended::Over(why) => {
                        let _ = events.send(RelayEvent::Down(why));
                        return;
                    }
                    Ended::Dropped => {}
                }
                stable = opened.elapsed() >= STABLE_AFTER;
                why = "the connection closed".to_string();
            }
            Err(e) => why = e.to_string(),
        }
        // A connection that opens and closes at once is a failure too, or the loop would never end.
        failures = if stable { 0 } else { failures + 1 };
        if failures >= MAX_FAILURES {
            let _ = events.send(RelayEvent::Down(format!("cannot reach the relay ({why})")));
            return;
        }
        let _ = events.send(RelayEvent::Reconnecting(why));
        // What was queued for the old connection is out of date.
        while cmds.try_recv().is_ok() {}
        tokio::time::sleep(backoff(failures)).await;
    }
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn session(
    cfg: &RelayConfig,
    ws: Ws,
    cmds: &mut mpsc::UnboundedReceiver<ClientMsg>,
    events: &mpsc::UnboundedSender<RelayEvent>,
    resume: &mut Option<Resume>,
) -> Ended {
    let (mut sink, mut source) = ws.split();
    let hello = ClientMsg::Hello { name: cfg.name.clone(), host_key: cfg.host_key.clone(), resume: resume.clone(), hue: cfg.hue };
    if send_msg(&mut sink, &hello).await.is_err() {
        return Ended::Dropped;
    }
    let mut me: Option<MemberId> = None;
    let mut keepalive = tokio::time::interval(KEEPALIVE);
    keepalive.tick().await;
    loop {
        tokio::select! {
            incoming = source.next() => {
                let Some(Ok(frame)) = incoming else { return Ended::Dropped };
                let Message::Text(text) = frame else {
                    if matches!(frame, Message::Close(_)) { return Ended::Dropped }
                    continue;
                };
                let Ok(msg) = serde_json::from_str::<ServerMsg>(text.as_str()) else { continue };
                match &msg {
                    ServerMsg::Welcome { you, token, host, .. } => {
                        me = Some(*you);
                        if !host {
                            *resume = Some(Resume { id: *you, token: token.clone() });
                        }
                    }
                    ServerMsg::Error { code: ErrorCode::NoRoom | ErrorCode::BadKey | ErrorCode::Full, message } => {
                        return Ended::Over(message.clone());
                    }
                    ServerMsg::Closed { reason } => return Ended::Over(reason.clone()),
                    ServerMsg::Left { id, reason: LeftReason::Kicked } if Some(*id) == me => {
                        return Ended::Over("the host removed you from the jam".into());
                    }
                    _ => {}
                }
                if events.send(RelayEvent::Msg(msg)).is_err() {
                    return Ended::Over("closed".into());
                }
            }
            cmd = cmds.recv() => {
                let Some(cmd) = cmd else { return Ended::Over("closed".into()) };
                if send_msg(&mut sink, &cmd).await.is_err() {
                    return Ended::Dropped;
                }
            }
            _ = keepalive.tick() => {
                if send_msg(&mut sink, &ClientMsg::Ping).await.is_err() {
                    return Ended::Dropped;
                }
            }
        }
    }
}

async fn send_msg<S>(sink: &mut S, msg: &ClientMsg) -> Result<(), ()>
where
    S: futures_util::Sink<Message> + Unpin,
{
    let text = serde_json::to_string(msg).map_err(|_| ())?;
    sink.send(Message::text(text)).await.map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_grow_and_stop_growing() {
        assert_eq!(backoff(0), Duration::from_millis(500));
        assert_eq!(backoff(2), Duration::from_secs(2));
        assert_eq!(backoff(4), Duration::from_secs(8));
        assert_eq!(backoff(30), Duration::from_secs(8));
    }
}
