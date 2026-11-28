//! Persistent, streaming QUIC connection between paired Macs (v0.6 "Use your other Mac").
//!
//! M0 transport spike (docs/plans/2026-11-28-use-your-other-mac.md §3, §9):
//! - `libp2p-stream` gives raw bidirectional QUIC streams per protocol
//!   (`Control::open_stream` / `Control::accept`), one stream per forwarded request;
//! - `ping` every 2 s measures RTT; a failed ping closes the connection;
//! - QUIC keep-alive 2 s and QUIC idle timeout 8 s → a sleeping / vanished peer is
//!   detected in ≤ 10 s (M0 measured idle + ~1.04 s; M1 decision: 8 s);
//! - the swarm keeps an idle connection (no open streams) for a long time, so the
//!   pinned peer connection stays up between editor requests (no handshake per request);
//! - connection limits: 4 per peer, 32 total, 8 pending inbound.
//!
//! Trust gate (M2): `p2p_forward::gate` (share side) and `p2p_forward::client` (use side)
//! close connections of untrusted peers; callers must only dial pinned `/p2p/<id>` addresses.

use std::time::Duration;

use libp2p::swarm::NetworkBehaviour;
use libp2p::{connection_limits, ping, StreamProtocol, Swarm, SwarmBuilder};

pub use libp2p_stream::{Control, IncomingStreams, OpenStreamError};

/// M0 raw byte pipe protocol (spike / bench reproduction only; never offered by `share`).
pub const PIPE_PROTOCOL_V0: StreamProtocol = StreamProtocol::new("/slipstream/pipe/0");
/// M1 allowlisted OpenAI forwarder: one stream per HTTP request.
pub const OPENAI_PROTOCOL_V1: StreamProtocol = StreamProtocol::new("/slipstream/openai/1");
/// M1 status RPC (`Status`, `StartModel`, `StopModel`).
pub const STATUS_PROTOCOL_V1: StreamProtocol = StreamProtocol::new("/slipstream/status/1");
/// M2 pairing (SPAKE2 code) and `Unpair`; the only protocol served to untrusted peers,
/// and only while a pairing window is open.
pub const PAIR_PROTOCOL_V1: StreamProtocol = StreamProtocol::new("/slipstream/pair/1");

/// Transport tuning for paired connections.
#[derive(Debug, Clone)]
pub struct PairedConfig {
    /// QUIC keep-alive (PING frames) and libp2p ping interval.
    pub keep_alive: Duration,
    /// QUIC max idle timeout: no packet from the peer for this long closes the connection.
    pub idle_timeout: Duration,
    /// libp2p ping timeout; a failure closes the connection (see [`PairedBehaviour`]).
    pub ping_timeout: Duration,
    /// How long the swarm keeps a connection without open streams.
    pub idle_connection_keep: Duration,
    pub max_established_per_peer: u32,
    pub max_established_total: u32,
    pub max_pending_incoming: u32,
}

impl Default for PairedConfig {
    fn default() -> Self {
        Self {
            keep_alive: Duration::from_secs(2),
            idle_timeout: Duration::from_secs(8),
            ping_timeout: Duration::from_secs(5),
            idle_connection_keep: Duration::from_secs(24 * 60 * 60),
            max_established_per_peer: 4,
            max_established_total: 32,
            max_pending_incoming: 8,
        }
    }
}

#[derive(NetworkBehaviour)]
#[behaviour(to_swarm = "PairedEvent")]
pub struct PairedBehaviour {
    pub limits: connection_limits::Behaviour,
    pub ping: ping::Behaviour,
    pub stream: libp2p_stream::Behaviour,
}

#[derive(Debug)]
pub enum PairedEvent {
    Ping(ping::Event),
}

impl From<ping::Event> for PairedEvent {
    fn from(event: ping::Event) -> Self {
        Self::Ping(event)
    }
}

impl From<std::convert::Infallible> for PairedEvent {
    fn from(never: std::convert::Infallible) -> Self {
        match never {}
    }
}

/// `libp2p_stream::Behaviour` emits `()` (it never produces events).
impl From<()> for PairedEvent {
    fn from(_: ()) -> Self {
        unreachable!("libp2p-stream emits no swarm events")
    }
}

impl PairedBehaviour {
    pub fn new(config: &PairedConfig) -> Self {
        let limits = connection_limits::ConnectionLimits::default()
            .with_max_established_per_peer(Some(config.max_established_per_peer))
            .with_max_established(Some(config.max_established_total))
            .with_max_pending_incoming(Some(config.max_pending_incoming));
        Self {
            limits: connection_limits::Behaviour::new(limits),
            ping: ping::Behaviour::new(
                ping::Config::new()
                    .with_interval(config.keep_alive)
                    .with_timeout(config.ping_timeout),
            ),
            stream: libp2p_stream::Behaviour::new(),
        }
    }
}

/// Build a QUIC swarm for paired-Mac streaming with a persistent identity.
pub fn build_paired_swarm(
    identity: libp2p::identity::Keypair,
    config: &PairedConfig,
) -> Swarm<PairedBehaviour> {
    let keep_alive = config.keep_alive;
    let idle_ms = u32::try_from(config.idle_timeout.as_millis()).unwrap_or(u32::MAX);
    let idle_keep = config.idle_connection_keep;
    let behaviour_config = config.clone();
    SwarmBuilder::with_existing_identity(identity)
        .with_tokio()
        .with_quic_config(|mut quic| {
            quic.keep_alive_interval = keep_alive;
            quic.max_idle_timeout = idle_ms;
            quic
        })
        .with_behaviour(|_| PairedBehaviour::new(&behaviour_config))
        .expect("infallible paired behaviour")
        .with_swarm_config(|swarm| swarm.with_idle_connection_timeout(idle_keep))
        .build()
}

/// Apply the "failed ping closes the connection" policy. Returns the RTT on success.
pub fn handle_ping(swarm: &mut Swarm<PairedBehaviour>, event: &ping::Event) -> Option<Duration> {
    match &event.result {
        Ok(rtt) => Some(*rtt),
        Err(_) => {
            swarm.close_connection(event.connection);
            None
        }
    }
}
