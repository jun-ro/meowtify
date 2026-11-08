//! One direct connection between two peers, with two data channels:
//!
//! - `control`: small messages (playback state, requests, permissions).
//! - `audio`: the song data, in chunks of at most [`CHUNK`] bytes.
//!
//! Both are reliable and ordered. The host calls [`Link::offer`], the guest [`Link::answer`].
//! The offer, the answer and the ICE candidates travel through the relay; the link itself
//! only produces and consumes them as strings.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};

use bytes::BytesMut;
use tokio::sync::mpsc;
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder, RTCIceCandidateInit, RTCIceTransportPolicy,
    RTCPeerConnectionIceEvent, RTCPeerConnectionState, RTCSessionDescription,
};
use webrtc::runtime::{default_runtime, Runtime};

use crate::ice::IceConfig;

/// The most bytes in one audio message. WebRTC data channels carry 16 KiB per message.
pub const CHUNK: usize = 16_000;
/// Bytes a channel may hold unsent before `send` waits. This is the back-pressure that keeps a
/// slow guest from making the host buffer a whole song.
const SEND_BUFFER: usize = 256 * 1024;

const CONTROL: &str = "control";
const AUDIO: &str = "audio";

#[derive(Debug)]
pub enum Error {
    Webrtc(String),
    /// A channel is not open yet, or the link is closed.
    NotOpen,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Webrtc(e) => write!(f, "{e}"),
            Error::NotOpen => write!(f, "the connection is not open"),
        }
    }
}

impl std::error::Error for Error {}

fn werr(e: impl std::fmt::Display) -> Error {
    Error::Webrtc(e.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// One of our ICE candidates, as JSON, for the other side's [`Link::add_candidate`].
    Candidate(String),
    /// Both channels are open.
    Open,
    /// The connection failed or ended.
    Closed,
    Control(Vec<u8>),
    Audio(Vec<u8>),
}

#[derive(Default)]
struct Shared {
    control: OnceLock<Arc<dyn DataChannel>>,
    audio: OnceLock<Arc<dyn DataChannel>>,
    opened: AtomicU8,
}

struct Handler {
    tx: mpsc::UnboundedSender<Event>,
    shared: Arc<Shared>,
    runtime: Arc<dyn Runtime>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        if let Ok(init) = event.candidate.to_json() {
            if let Ok(text) = serde_json::to_string(&init) {
                let _ = self.tx.send(Event::Candidate(text));
            }
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        if matches!(state, RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed) {
            let _ = self.tx.send(Event::Closed);
        }
    }

    async fn on_data_channel(&self, dc: Arc<dyn DataChannel>) {
        pump(&self.runtime, dc, self.shared.clone(), self.tx.clone());
    }
}

/// Reads the events of one channel into the link's event stream.
fn pump(runtime: &Arc<dyn Runtime>, dc: Arc<dyn DataChannel>, shared: Arc<Shared>, tx: mpsc::UnboundedSender<Event>) {
    runtime.spawn(Box::pin(async move {
        let label = dc.label().await.unwrap_or_default();
        let is_control = match label.as_str() {
            CONTROL => true,
            AUDIO => false,
            _ => return, // not one of ours
        };
        let slot = if is_control { &shared.control } else { &shared.audio };
        let _ = slot.set(dc.clone());
        while let Some(event) = dc.poll().await {
            match event {
                DataChannelEvent::OnOpen => {
                    if shared.opened.fetch_add(1, Ordering::SeqCst) == 1 {
                        let _ = tx.send(Event::Open);
                    }
                }
                DataChannelEvent::OnMessage(m) => {
                    let data = m.data.to_vec();
                    let _ = tx.send(if is_control { Event::Control(data) } else { Event::Audio(data) });
                }
                DataChannelEvent::OnClose => {
                    let _ = tx.send(Event::Closed);
                    break;
                }
                _ => {}
            }
        }
    }));
}

/// Where a link listens for the direct connection.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub ice: IceConfig,
    /// UDP addresses to bind, such as `127.0.0.1:0`. Empty means this machine's main address.
    pub bind: Vec<String>,
}

/// The address of the interface that reaches the internet, found without sending anything.
fn main_address() -> String {
    let ip = std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect("192.0.2.1:9").and_then(|_| s.local_addr()))
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".into());
    format!("{ip}:0")
}

pub struct Link {
    pc: Arc<dyn PeerConnection>,
    shared: Arc<Shared>,
    events: Option<mpsc::UnboundedReceiver<Event>>,
}

impl Link {
    async fn build(opts: Options) -> Result<(Link, mpsc::UnboundedSender<Event>, Arc<dyn Runtime>), Error> {
        let runtime = default_runtime().ok_or_else(|| Error::Webrtc("no async runtime".into()))?;
        let (tx, events) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared::default());
        let bind = if opts.bind.is_empty() { vec![main_address()] } else { opts.bind };
        let pc = PeerConnectionBuilder::new()
            .with_configuration(RTCConfigurationBuilder::new()
                    .with_ice_servers(opts.ice.servers)
                    .with_ice_transport_policy(if opts.ice.relay_only { RTCIceTransportPolicy::Relay } else { RTCIceTransportPolicy::All })
                    .build())
            .with_handler(Arc::new(Handler { tx: tx.clone(), shared: shared.clone(), runtime: runtime.clone() }))
            .with_runtime(runtime.clone())
            .with_udp_addrs(bind)
            .with_data_channel_send_buffer_limit(SEND_BUFFER)
            .build()
            .await
            .map_err(werr)?;
        let pc: Arc<dyn PeerConnection> = Arc::new(pc);
        Ok((Link { pc, shared, events: Some(events) }, tx, runtime))
    }

    /// The host's side: opens both channels and returns the offer (SDP) to send to the guest.
    pub async fn offer(opts: Options) -> Result<(Link, String), Error> {
        let (link, tx, runtime) = Link::build(opts).await?;
        for label in [CONTROL, AUDIO] {
            let dc = link.pc.create_data_channel(label, None).await.map_err(werr)?;
            pump(&runtime, dc, link.shared.clone(), tx.clone());
        }
        let offer = link.pc.create_offer(None).await.map_err(werr)?;
        link.pc.set_local_description(offer.clone()).await.map_err(werr)?;
        Ok((link, offer.sdp))
    }

    /// The guest's side: takes the host's offer and returns the answer (SDP).
    pub async fn answer(opts: Options, offer_sdp: &str) -> Result<(Link, String), Error> {
        let (link, _, _) = Link::build(opts).await?;
        let offer = RTCSessionDescription::offer(offer_sdp.to_string()).map_err(werr)?;
        link.pc.set_remote_description(offer).await.map_err(werr)?;
        let answer = link.pc.create_answer(None).await.map_err(werr)?;
        link.pc.set_local_description(answer.clone()).await.map_err(werr)?;
        Ok((link, answer.sdp))
    }

    /// The host takes the guest's answer.
    pub async fn set_answer(&self, answer_sdp: &str) -> Result<(), Error> {
        let answer = RTCSessionDescription::answer(answer_sdp.to_string()).map_err(werr)?;
        self.pc.set_remote_description(answer).await.map_err(werr)
    }

    /// An ICE candidate of the other side, as JSON from its [`Event::Candidate`].
    pub async fn add_candidate(&self, json: &str) -> Result<(), Error> {
        let init: RTCIceCandidateInit = serde_json::from_str(json).map_err(werr)?;
        self.pc.add_ice_candidate(init).await.map_err(werr)
    }

    pub async fn send_control(&self, data: &[u8]) -> Result<(), Error> {
        let dc = self.shared.control.get().ok_or(Error::NotOpen)?;
        dc.send(BytesMut::from(data)).await.map_err(|_| Error::NotOpen)
    }

    /// Sends one audio chunk (at most [`CHUNK`] bytes). Waits while the channel is full.
    pub async fn send_audio(&self, data: &[u8]) -> Result<(), Error> {
        debug_assert!(data.len() <= CHUNK);
        let dc = self.shared.audio.get().ok_or(Error::NotOpen)?;
        dc.send(BytesMut::from(data)).await.map_err(|_| Error::NotOpen)
    }

    /// The next event. `None` once the link is gone, or after [`Link::take_events`].
    pub async fn next(&mut self) -> Option<Event> {
        self.events.as_mut()?.recv().await
    }

    /// Takes the event stream out, so the link can be shared (it sends through `&self`) while
    /// another task reads the events.
    pub fn take_events(&mut self) -> Option<mpsc::UnboundedReceiver<Event>> {
        self.events.take()
    }

    pub async fn close(&self) {
        let _ = self.pc.close().await;
    }
}
