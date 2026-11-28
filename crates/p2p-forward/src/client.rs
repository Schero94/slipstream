//! Use side ("MacBook", plan §3 `client.rs`): serves the editor on `127.0.0.1:<port>`
//! **and** `[::1]:<port>` (both or neither, so another local user cannot grab the other
//! address family of `localhost`) and forwards each allowed request over one stream of a
//! persistent, pinned QUIC connection to the share side.
//!
//! Local guards (before anything leaves this Mac): loopback bind only, `Host`
//! allowlist and browser refusal (any `Origin`, incl. `null`; any `Sec-Fetch-Site` but
//! `none` — DNS rebinding / web pages), optional per-install bearer token
//! ([`UseConfig::token`], `Authorization: Bearer <token>`, constant-time compare; the app
//! writes it into the editor configs as the "API key"), method/path allowlist and target
//! byte check, credential + hop-by-hop header stripping, 32 MiB streamed body cap, at
//! most [`LocalLimits::max_connections`] concurrent editor connections, 30 s idle
//! timeout while reading a request body.
//! Latency: TCP_NODELAY, one flush per response frame, one pre-opened + handshaken
//! spare stream per connection (hides stream negotiation after idle, M0 finding).
//! Cancellation: when the editor closes its connection (stop button), a `Cancel`
//! frame is sent and the stream dropped; the share side drops its upstream connection.
//! Trust (M2): only a peer pinned in the trust store is dialed; every stream re-checks
//! [`PeerTrust::trusted_key`] and uses the pinned static key for the authenticated
//! SealedStream; when the peer loses trust the connection is closed (after a best-effort
//! goodbye) and not re-dialed.

use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use libp2p::multiaddr::Protocol;
use libp2p::swarm::SwarmEvent;
use libp2p::{Multiaddr, PeerId};
use p2p_crypto::{NodeKeypair, StreamRole};
use p2p_libp2p::paired::{build_paired_swarm, handle_ping, Control, PairedConfig, PairedEvent};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;

use crate::allowlist;
use crate::http::{self, RequestHead};
use crate::proto::{
    channel_context, open_channel, BodyKind, ChannelReader, ChannelWriter, FrameType, ReqHead,
    Versions, WireError, BODY_CHUNK, OPENAI_PROTOCOL_V1, STATUS_PROTOCOL_V1,
};
use crate::status::{StatusReply, StatusRequest};
use crate::trust::{PeerTrust, Role, NOTIFY_REVOKED_BUDGET};
use crate::unix_ms;

pub const DIAL_TIMEOUT: Duration = Duration::from_secs(3);
pub const STREAM_OPEN_TIMEOUT: Duration = Duration::from_secs(2);
/// Streamed request body cap (plan §3), enforced on both ends.
pub use crate::proto::MAX_BODY;
/// The editor must send its request head within this time (slow-loris guard).
pub const HEAD_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct UseConfig {
    /// Pinned share address, must end in `/p2p/<PeerId>`; that PeerId must be trusted.
    pub peer: Multiaddr,
    /// M2 trust store view (the paired Macs).
    pub trust: Arc<dyn PeerTrust>,
    /// This node's static X25519 key (`node.key`), for the authenticated SealedStream.
    pub node: NodeKeypair,
    /// Local editor address; must be loopback.
    pub bind: SocketAddr,
    pub paired: PairedConfig,
    /// Keep one pre-opened spare stream (default on; off only for A/B measurements).
    pub spare_stream: bool,
    /// Per-install bearer token: when set, every editor request must carry
    /// `Authorization: Bearer <token>` (checked before the header is stripped).
    pub token: Option<String>,
    pub local: LocalLimits,
    /// M3: live candidate addresses (LAN discovery + trust store) and connection events.
    /// `None` = only `peer` is dialed (pre-M3 behaviour).
    pub discovery: Option<UseDiscovery>,
    /// The Slipstream app's own window (v0.6 M4), e.g. `tauri://localhost`: requests with
    /// exactly this `Origin` are not refused as browser requests (they still need the bearer
    /// token, so `token` must be set), CORS preflights from it are answered locally, and its
    /// responses carry `Access-Control-Allow-Origin` for it. No web page can send this origin.
    pub app_origin: Option<String>,
    /// M5: bind the editor port and return at once, even if the paired Mac cannot be
    /// reached yet; keep dialing in the background. While it is unreachable, editors get
    /// a clear 503 JSON error ([`UseHandle::set_offline`]) instead of "connection refused".
    /// `false` = the first dial must succeed (the pre-M5 behaviour).
    pub persistent: bool,
    /// How long a request waits for the connection to come back before the 503.
    pub connect_wait: Duration,
    /// Protocol version spoken / oldest accepted (M5).
    pub versions: Versions,
    /// Called with the swarm's stream control before the first dial, so extra acceptors
    /// (the pair protocol, which carries `Unpair`) exist before any connection can carry a
    /// stream for them (libp2p-stream drops inbound streams without an acceptor).
    pub before_dial: Option<BeforeDial>,
    pub verbose: bool,
}

/// See [`UseConfig::before_dial`].
pub type BeforeDial = Arc<dyn Fn(Control) + Send + Sync>;

/// Default [`UseConfig::connect_wait`].
pub const CONNECT_WAIT: Duration = Duration::from_secs(2);

/// What editors get while the paired Mac cannot be reached (persistent mode).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalAnswer {
    pub status: u16,
    /// `error.slipstream_code` in the JSON body.
    pub code: String,
    pub message: String,
}

impl LocalAnswer {
    pub fn new(status: u16, code: &str, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
        }
    }

    fn bytes(&self) -> Vec<u8> {
        http::error_response(self.status, &self.code, &self.message)
    }
}

/// Candidate addresses of the pinned peer (M3). Every address is dialed only with the
/// pinned `/p2p/<PeerId>` suffix; an address pinning another id is dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AddrBook {
    /// Currently announced on the LAN (mDNS, untrusted). When non-empty and the live
    /// connection's address is not among them (e.g. the share restarted on a new port),
    /// they are dialed at once; a new connection that authenticates replaces the old one.
    pub discovered: Vec<Multiaddr>,
    /// Last addresses that worked (trust store `last_addrs`): dialed while disconnected.
    pub known: Vec<Multiaddr>,
}

impl AddrBook {
    fn all(&self) -> impl Iterator<Item = &Multiaddr> {
        self.discovered.iter().chain(self.known.iter())
    }
}

/// What the use side reports to its owner (M3: record `last_addrs`, re-browse).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UseEvent {
    /// A connection to the pinned peer authenticated at `addr` (without `/p2p`).
    Connected { addr: Multiaddr },
    /// A dial attempt failed (unreachable, or an impostor at that address).
    DialFailed { error: String },
    /// The last connection closed.
    Disconnected,
}

#[derive(Clone)]
pub struct UseDiscovery {
    pub addrs: watch::Receiver<AddrBook>,
    pub events: tokio::sync::mpsc::UnboundedSender<UseEvent>,
}

/// Extra wait for a first LAN answer when the use side starts with discovery.
pub const DISCOVERY_WAIT: Duration = Duration::from_secs(3);

/// `addr` without `/p2p/...`.
pub fn strip_p2p(addr: &Multiaddr) -> Multiaddr {
    addr.iter().filter(|p| !matches!(p, Protocol::P2p(_))).collect()
}

/// Keep only dialable addresses for `peer`, each ending in `/p2p/<peer>` (appended when
/// missing). Addresses pinning another PeerId or without a transport part are dropped.
pub fn pinned_candidates<'a>(peer: PeerId, addrs: impl IntoIterator<Item = &'a Multiaddr>) -> Vec<Multiaddr> {
    let mut out: Vec<Multiaddr> = Vec::new();
    for a in addrs {
        let pinned_other = a.iter().any(|p| matches!(p, Protocol::P2p(id) if id != peer));
        let bare = strip_p2p(a);
        if pinned_other || bare.is_empty() {
            continue;
        }
        let full = bare.with(Protocol::P2p(peer));
        if !out.contains(&full) {
            out.push(full);
        }
    }
    out
}

async fn book_changed(rx: &mut Option<watch::Receiver<AddrBook>>) -> AddrBook {
    match rx {
        Some(r) => match r.changed().await {
            Ok(()) => r.borrow_and_update().clone(),
            Err(_) => {
                *rx = None;
                std::future::pending().await
            }
        },
        None => std::future::pending().await,
    }
}

fn dial_candidates(
    swarm: &mut libp2p::Swarm<p2p_libp2p::paired::PairedBehaviour>,
    peer: PeerId,
    addrs: Vec<Multiaddr>,
    always: bool,
    verbose: bool,
) {
    use libp2p::swarm::dial_opts::{DialOpts, PeerCondition};
    if addrs.is_empty() {
        return;
    }
    if verbose {
        eprintln!("use event=dial peer={peer} addrs={addrs:?} always={always}");
    }
    let cond = if always { PeerCondition::Always } else { PeerCondition::Disconnected };
    if let Err(e) = swarm.dial(DialOpts::peer_id(peer).addresses(addrs).condition(cond).build()) {
        if verbose {
            eprintln!("use event=dial_error error={e}");
        }
    }
}

/// Limits of the local editor listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalLimits {
    /// Concurrent editor connections; more are answered 503 at once.
    pub max_connections: usize,
    /// Longest pause while reading a request body before 408.
    pub body_idle_timeout: Duration,
}

impl Default for LocalLimits {
    fn default() -> Self {
        Self {
            max_connections: 16,
            body_idle_timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct UseStats {
    pub connections_established: u64,
    pub connections_closed: u64,
    pub ping_rtt_us: Vec<u64>,
    pub stream_open_us: Vec<u64>,
    pub spare_hits: u64,
    pub spare_misses: u64,
    pub requests: u64,
    pub refused_locally: u64,
    /// Answered locally with the offline / blocked answer (M5).
    pub answered_offline: u64,
    pub unauthorized: u64,
    pub connections_over_cap: u64,
    pub cancels_sent: u64,
    pub peer_lost_mid_request: u64,
}

pub type SharedUseStats = Arc<Mutex<UseStats>>;

pub struct UseHandle {
    pub peer_id: PeerId,
    /// The configured loopback address (port resolved).
    pub bind: SocketAddr,
    /// The other loopback family on the same port (`[::1]` for `127.0.0.1` and back).
    pub bind_companion: SocketAddr,
    pub stats: SharedUseStats,
    shared: Arc<Shared>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for UseHandle {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl UseHandle {
    /// One status RPC to the share side.
    pub async fn status(&self) -> io::Result<StatusReply> {
        self.shared.rpc(StatusRequest::Status).await
    }

    pub async fn rpc(&self, req: StatusRequest) -> io::Result<StatusReply> {
        self.shared.rpc(req).await
    }

    /// Whether the paired connection is currently up.
    pub fn connected(&self) -> bool {
        self.shared.connection.borrow().is_some()
    }

    /// The swarm's stream control (to register more protocols, e.g. `/slipstream/pair/1`).
    pub fn control(&self) -> Control {
        self.shared.control.lock().unwrap().clone()
    }

    /// The answer for editors while the paired Mac is unreachable (waking / asleep).
    pub fn set_offline(&self, answer: LocalAnswer) {
        *self.shared.offline.lock().unwrap() = answer;
    }

    /// Answer every editor request locally with this (e.g. a protocol version mismatch),
    /// even while connected; `None` = forward again.
    pub fn set_blocked(&self, answer: Option<LocalAnswer>) {
        *self.shared.blocked.lock().unwrap() = answer;
    }

    /// The last dial error (unreachable, or an impostor at a discovered address).
    pub fn last_error(&self) -> Option<String> {
        self.shared.last_error.lock().unwrap().clone()
    }

    /// Wait until the paired connection is up (or `timeout`).
    pub async fn wait_connected(&self, timeout: Duration) -> bool {
        let mut rx = self.shared.connection.clone();
        tokio::time::timeout(timeout, rx.wait_for(|g| g.is_some())).await.is_ok_and(|r| r.is_ok())
    }
}

type Channel = (ChannelReader, ChannelWriter);

struct Spare {
    generation: u64,
    channel: Channel,
}

struct Shared {
    local: PeerId,
    peer: PeerId,
    node: NodeKeypair,
    trust: Arc<dyn PeerTrust>,
    control: Mutex<Control>,
    /// `Some(generation)` while connected; generation increments per connection.
    connection: watch::Receiver<Option<u64>>,
    spare: Mutex<Option<Spare>>,
    spare_wanted: Notify,
    spare_enabled: bool,
    stats: SharedUseStats,
    token: Option<String>,
    app_origin: Option<String>,
    local_limits: LocalLimits,
    offline: Mutex<LocalAnswer>,
    blocked: Mutex<Option<LocalAnswer>>,
    last_error: Arc<Mutex<Option<String>>>,
    connect_wait: Duration,
    versions: Versions,
    verbose: bool,
}

fn other<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(e.to_string())
}

impl Shared {
    async fn open(&self, protocol: libp2p::StreamProtocol) -> io::Result<Channel> {
        // Per-stream trust check: a revoked peer gets no new stream.
        let Some(peer_key) = self.trust.trusted_key_for(&self.peer, Role::Share) else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "that Mac is no longer paired with this one",
            ));
        };
        let mut control = self.control.lock().unwrap().clone();
        let t0 = Instant::now();
        let stream = tokio::time::timeout(STREAM_OPEN_TIMEOUT, control.open_stream(self.peer, protocol.clone()))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "stream open timed out"))?
            .map_err(|e| match e {
                // M5: the other Mac does not speak this protocol (another version).
                p2p_libp2p::paired::OpenStreamError::UnsupportedProtocol(p) => io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("the other Mac does not support {p} (different Slipstream version)"),
                ),
                other_err => other(other_err),
            })?;
        let context = channel_context(&self.local, &self.peer, &protocol);
        let channel = tokio::time::timeout(
            STREAM_OPEN_TIMEOUT,
            open_channel(stream, StreamRole::Initiator, &context, &self.node, &peer_key),
        )
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "stream handshake timed out"))??;
        let us = t0.elapsed().as_micros() as u64;
        self.stats.lock().unwrap().stream_open_us.push(us);
        if self.verbose {
            eprintln!("use stream_open_us={us}");
        }
        Ok(channel)
    }

    /// A ready OpenAI channel: the spare if it belongs to the live connection, else a
    /// fresh stream. Asks the refill task for a new spare either way.
    async fn take_channel(&self) -> io::Result<Channel> {
        let generation = *self.connection.borrow();
        let spare = self.spare.lock().unwrap().take();
        if self.spare_enabled {
            self.spare_wanted.notify_one();
        }
        if let (Some(spare), Some(g)) = (spare, generation) {
            if spare.generation == g {
                self.stats.lock().unwrap().spare_hits += 1;
                return Ok(spare.channel);
            }
        }
        self.stats.lock().unwrap().spare_misses += 1;
        self.open(OPENAI_PROTOCOL_V1).await
    }

    async fn rpc(&self, req: StatusRequest) -> io::Result<StatusReply> {
        let (mut reader, mut writer) = self.open(STATUS_PROTOCOL_V1).await?;
        writer
            .send(FrameType::StatusReq, &crate::status::encode_request(req, self.versions))
            .await?;
        let frame = tokio::time::timeout(Duration::from_secs(10), reader.next())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "status timed out"))??
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "no status reply"))?;
        if frame.kind != FrameType::StatusResp {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "expected StatusResp"));
        }
        let _ = writer.finish().await;
        serde_json::from_slice(&frame.payload).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

/// Keeps one spare OpenAI stream (opened + SealedStream handshake done) per live
/// connection.
async fn spare_refill(shared: Arc<Shared>) {
    let mut conn = shared.connection.clone();
    loop {
        let generation = *conn.borrow_and_update();
        let have = shared
            .spare
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|s| Some(s.generation) == generation);
        if generation.is_none() || have {
            // Wait for a reconnect or a taken spare.
            tokio::select! {
                _ = conn.changed() => {}
                _ = shared.spare_wanted.notified() => {}
            }
            continue;
        }
        let g = generation.unwrap();
        match shared.open(OPENAI_PROTOCOL_V1).await {
            Ok(channel) => {
                if *shared.connection.borrow() == Some(g) {
                    *shared.spare.lock().unwrap() = Some(Spare {
                        generation: g,
                        channel,
                    });
                }
            }
            Err(e) => {
                if shared.verbose {
                    eprintln!("use event=spare_open_error error={e}");
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

/// Dial the pinned peer, keep the connection (re-dial on loss), serve `bind`.
pub async fn start_use(identity: libp2p::identity::Keypair, cfg: UseConfig) -> io::Result<UseHandle> {
    let Some(Protocol::P2p(peer)) = cfg.peer.iter().last() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "peer address must end in /p2p/<peer id> (pinned dialing only)",
        ));
    };
    if !cfg.bind.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the editor address must be loopback (127.0.0.1)",
        ));
    }
    if cfg.trust.trusted_key_for(&peer, Role::Share).is_none() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{peer} is not a paired Mac that shares its model with this one (pair first: slipstream-node pair --peer <addr> --code <code>)"),
        ));
    }
    if cfg.token.as_deref().is_some_and(|t| t.len() < 16 || !t.bytes().all(|b| b.is_ascii_graphic())) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the local token must be at least 16 visible ASCII characters",
        ));
    }
    if cfg.app_origin.is_some() && cfg.token.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "app_origin needs a local token (the app window must authenticate like an editor)",
        ));
    }
    if cfg
        .app_origin
        .as_deref()
        .is_some_and(|o| o.is_empty() || o == "null" || !o.bytes().all(|b| b.is_ascii_graphic()))
    {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "bad app_origin"));
    }
    let (listener, listener_companion) = bind_loopback_pair(cfg.bind).await?;
    let bind = listener.local_addr()?;
    let bind_companion = listener_companion.local_addr()?;

    let mut swarm = build_paired_swarm(identity, &cfg.paired);
    let local = *swarm.local_peer_id();
    // Pinned dialing only: `cfg.peer` (if it has a transport part) plus the address book,
    // every candidate with the `/p2p/<peer>` suffix.
    let configured = pinned_candidates(peer, std::iter::once(&cfg.peer));
    let (mut book_rx, events) = match &cfg.discovery {
        Some(d) => (Some(d.addrs.clone()), Some(d.events.clone())),
        None => (None, None),
    };
    let emit = move |e: UseEvent| {
        if let Some(tx) = &events {
            let _ = tx.send(e);
        }
    };
    let mut book = book_rx.as_mut().map(|r| r.borrow_and_update().clone()).unwrap_or_default();
    let candidates = move |book: &AddrBook| pinned_candidates(peer, configured.iter().chain(book.all()));
    let first = candidates(&book);
    if first.is_empty() && book_rx.is_none() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "no address to dial for the paired Mac"));
    }
    if let Some(hook) = &cfg.before_dial {
        hook(swarm.behaviour().stream.new_control());
    }
    dial_candidates(&mut swarm, peer, first.clone(), false, cfg.verbose);
    let started = Instant::now();
    let wait = if book_rx.is_some() { DIAL_TIMEOUT + DISCOVERY_WAIT } else { DIAL_TIMEOUT };
    let mut last_error: Option<String> = None;
    let mut dialed: Vec<Multiaddr> = first.clone();
    let shared_error: Arc<Mutex<Option<String>>> = Arc::default();
    let first_conn = if cfg.persistent {
        // M5: serve at once; the swarm task below keeps dialing.
        None
    } else {
        Some(tokio::time::timeout(wait, async {
        loop {
            tokio::select! {
                event = swarm.select_next_some() => match event {
                    SwarmEvent::ConnectionEstablished { peer_id, connection_id, endpoint, .. } if peer_id == peer => {
                        return Ok((connection_id, strip_p2p(endpoint.get_remote_address())));
                    }
                    SwarmEvent::ConnectionEstablished { connection_id, .. } => {
                        swarm.close_connection(connection_id);
                    }
                    SwarmEvent::OutgoingConnectionError { error, .. } => {
                        let msg = error.to_string();
                        emit(UseEvent::DialFailed { error: msg.clone() });
                        if book_rx.is_none() {
                            return Err(other(msg));
                        }
                        last_error = Some(msg);
                    }
                    _ => {}
                },
                b = book_changed(&mut book_rx) => {
                    book = b;
                    // only addresses not tried yet (records trickle in one by one)
                    let c: Vec<Multiaddr> = candidates(&book).into_iter().filter(|a| !dialed.contains(a)).collect();
                    dialed.extend(c.iter().cloned());
                    dial_candidates(&mut swarm, peer, c, false, cfg.verbose);
                }
            }
        }
    })
    .await)
    };
    let first = match first_conn {
        None => None,
        Some(Ok(Ok(c))) => Some(c),
        Some(Ok(Err(e))) => return Err(e),
        Some(Err(_)) => {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                match last_error {
                    Some(e) => format!("could not reach the paired Mac: {e}"),
                    None => "dial timed out (is the other Mac awake and sharing?)".into(),
                },
            ))
        }
    };
    let stats = SharedUseStats::default();
    if let Some((_, first_addr)) = &first {
        eprintln!(
            "use event=connected peer={peer} addr={first_addr} dial_ms={:.2} unix_ms={}",
            started.elapsed().as_secs_f64() * 1e3,
            unix_ms()
        );
        emit(UseEvent::Connected { addr: first_addr.clone() });
        stats.lock().unwrap().connections_established += 1;
    } else {
        eprintln!("use event=waiting_for_peer peer={peer} unix_ms={}", unix_ms());
    }
    let (conn_tx, conn_rx) = watch::channel(first.as_ref().map(|_| 1u64));
    let control = swarm.behaviour().stream.new_control();
    let shared = Arc::new(Shared {
        local,
        peer,
        node: cfg.node.clone(),
        trust: cfg.trust.clone(),
        control: Mutex::new(control),
        connection: conn_rx,
        spare: Mutex::new(None),
        spare_wanted: Notify::new(),
        spare_enabled: cfg.spare_stream,
        stats: stats.clone(),
        token: cfg.token.clone(),
        app_origin: cfg.app_origin.clone(),
        local_limits: cfg.local,
        offline: Mutex::new(LocalAnswer::new(
            503,
            "peer_unreachable",
            "The other Mac is not reachable right now. Slipstream keeps trying; try again in a few seconds.",
        )),
        blocked: Mutex::new(None),
        last_error: shared_error.clone(),
        connect_wait: cfg.connect_wait,
        versions: cfg.versions,
        verbose: cfg.verbose,
    });

    let swarm_stats = stats.clone();
    let verbose = cfg.verbose;
    let trust = cfg.trust.clone();
    let notify_control = shared.control.lock().unwrap().clone();
    let swarm_error = shared_error.clone();
    let swarm_task = tokio::spawn(async move {
        let mut generation: u64 = if first.is_some() { 1 } else { 0 };
        // An address-change dial is in flight: its connection replaces the old ones.
        // Any other extra connection (late duplicates of the start-up race) is closed.
        let mut switch_pending = false;
        // live connections to the pinned peer → remote address (without /p2p)
        let mut conns: std::collections::HashMap<libp2p::swarm::ConnectionId, Multiaddr> =
            first.into_iter().collect();
        let mut redial = tokio::time::interval(Duration::from_secs(2));
        redial.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut epoch = trust.subscribe();
        let mut epoch_live = true;
        let mut last_key = trust.trusted_key_for(&peer, Role::Share);
        // Peer lost trust: no re-dial until it is trusted again.
        let mut suspended = false;
        let (close_tx, mut close_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        loop {
            tokio::select! {
                changed = epoch.changed(), if epoch_live => {
                    if changed.is_err() {
                        epoch_live = false;
                    }
                    match (trust.trusted_key_for(&peer, Role::Share), last_key) {
                        (None, Some(old)) => {
                            last_key = None;
                            suspended = true;
                            let _ = conn_tx.send(None);
                            eprintln!("use event=revoked peer={peer} unix_ms={}", unix_ms());
                            let notify = trust.notify_revoked(peer, old, notify_control.clone());
                            let close_tx = close_tx.clone();
                            tokio::spawn(async move {
                                let _ = tokio::time::timeout(NOTIFY_REVOKED_BUDGET, notify).await;
                                let _ = close_tx.send(());
                            });
                        }
                        (Some(k), _) => {
                            last_key = Some(k);
                            suspended = false;
                        }
                        (None, None) => {}
                    }
                }
                Some(()) = close_rx.recv() => {
                    if trust.trusted_key_for(&peer, Role::Share).is_none() {
                        let _ = swarm.disconnect_peer_id(peer);
                    }
                }
                b = book_changed(&mut book_rx) => {
                    book = b;
                    if suspended {
                        continue;
                    }
                    if conns.is_empty() {
                        let c = candidates(&book);
                        dial_candidates(&mut swarm, peer, c, false, verbose);
                    } else if !book.discovered.is_empty() {
                        // Connected, but is the share still where we are connected? If
                        // its announced addresses no longer include ours (restart on a
                        // new port, new IP), dial them now; the old connection is only
                        // replaced once a new one authenticates as the pinned peer.
                        let announced: Vec<Multiaddr> = book.discovered.iter().map(strip_p2p).collect();
                        if !conns.values().any(|a| announced.contains(a)) {
                            let c = pinned_candidates(peer, book.discovered.iter());
                            eprintln!("use event=address_changed peer={peer} unix_ms={}", unix_ms());
                            switch_pending = true;
                            dial_candidates(&mut swarm, peer, c, true, verbose);
                        }
                    }
                }
                event = swarm.select_next_some() => match event {
                    SwarmEvent::Behaviour(PairedEvent::Ping(event)) => {
                        if let Some(rtt) = handle_ping(&mut swarm, &event) {
                            swarm_stats.lock().unwrap().ping_rtt_us.push(rtt.as_micros() as u64);
                            if verbose {
                                eprintln!("use ping peer={} rtt_us={}", event.peer, rtt.as_micros());
                            }
                        } else {
                            eprintln!("use event=ping_failed peer={} unix_ms={}", event.peer, unix_ms());
                        }
                    }
                    SwarmEvent::ConnectionEstablished { peer_id, connection_id, .. } if peer_id != peer || suspended => {
                        // Only the pinned, still-trusted peer may hold a connection.
                        swarm.close_connection(connection_id);
                    }
                    SwarmEvent::ConnectionEstablished { connection_id, .. } if !conns.is_empty() && !switch_pending => {
                        // a duplicate: keep the connection we already use
                        swarm.close_connection(connection_id);
                    }
                    SwarmEvent::ConnectionEstablished { peer_id, connection_id, endpoint, .. } if peer_id == peer => {
                        switch_pending = false;
                        let addr = strip_p2p(endpoint.get_remote_address());
                        // The newest authenticated connection wins (it proved the current
                        // address); older ones are closed so streams use the live one.
                        let olds: Vec<_> = conns.keys().copied().collect();
                        conns.insert(connection_id, addr.clone());
                        for old in &olds {
                            swarm.close_connection(*old);
                        }
                        generation += 1;
                        *swarm_error.lock().unwrap() = None;
                        swarm_stats.lock().unwrap().connections_established += 1;
                        let _ = conn_tx.send(Some(generation));
                        eprintln!(
                            "use event=connected peer={peer_id} addr={addr} replaced={} unix_ms={}",
                            olds.len(),
                            unix_ms()
                        );
                        emit(UseEvent::Connected { addr });
                    }
                    SwarmEvent::ConnectionClosed { peer_id, connection_id, cause, .. } if peer_id == peer => {
                        let was_live = conns.remove(&connection_id).is_some();
                        if was_live && conns.is_empty() {
                            swarm_stats.lock().unwrap().connections_closed += 1;
                            let _ = conn_tx.send(None);
                            eprintln!("use event=closed peer={peer_id} cause={cause:?} unix_ms={}", unix_ms());
                            emit(UseEvent::Disconnected);
                        }
                    }
                    SwarmEvent::OutgoingConnectionError { peer_id, error, .. } if peer_id.is_none() || peer_id == Some(peer) => {
                        if verbose {
                            eprintln!("use event=dial_failed error={error}");
                        }
                        *swarm_error.lock().unwrap() = Some(error.to_string());
                        emit(UseEvent::DialFailed { error: error.to_string() });
                    }
                    _ => {}
                },
                _ = redial.tick() => {
                    if !suspended && conns.is_empty() && !swarm.is_connected(&peer) {
                        let c = candidates(&book);
                        dial_candidates(&mut swarm, peer, c, false, verbose);
                    }
                }
            }
        }
    });

    let mut tasks = vec![swarm_task];
    if cfg.spare_stream {
        tasks.push(tokio::spawn(spare_refill(shared.clone())));
    }
    let conn_cap = Arc::new(tokio::sync::Semaphore::new(cfg.local.max_connections.max(1)));
    for listener in [listener, listener_companion] {
        let accept_shared = shared.clone();
        let conn_cap = conn_cap.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    continue;
                };
                let shared = accept_shared.clone();
                let Ok(permit) = conn_cap.clone().try_acquire_owned() else {
                    shared.stats.lock().unwrap().connections_over_cap += 1;
                    tokio::spawn(async move {
                        let mut tcp = tcp;
                        let _ = tokio::time::timeout(
                            Duration::from_secs(2),
                            tcp.write_all(&http::error_response(
                                503,
                                "too_many_connections",
                                "Too many open connections to the forwarder. Try again in a few seconds.",
                            )),
                        )
                        .await;
                        let _ = tcp.shutdown().await;
                    });
                    continue;
                };
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Err(e) = serve_connection(&shared, tcp, bind.port()).await {
                        if shared.verbose {
                            eprintln!("use event=request_error error={e}");
                        }
                    }
                });
            }
        }));
    }
    eprintln!("use event=serving bind=http://{bind}/v1 also=http://{bind_companion}/v1 peer={peer} token={}", if cfg.token.is_some() { "required" } else { "off" });
    Ok(UseHandle {
        peer_id: peer,
        bind,
        bind_companion,
        stats,
        shared,
        tasks,
    })
}

/// The other loopback family for `addr` (`127.0.0.1` ↔ `[::1]`).
fn companion_ip(addr: SocketAddr) -> std::net::IpAddr {
    match addr {
        SocketAddr::V4(_) => std::net::Ipv6Addr::LOCALHOST.into(),
        SocketAddr::V6(_) => std::net::Ipv4Addr::LOCALHOST.into(),
    }
}

/// Bind the loopback address and its other family on the same port. Refuses to start if
/// either is taken (an editor resolving `localhost` to the taken one would talk to
/// someone else). Port 0: retried with fresh ports.
async fn bind_loopback_pair(addr: SocketAddr) -> io::Result<(TcpListener, TcpListener)> {
    let attempts = if addr.port() == 0 { 16 } else { 1 };
    let mut last_err = None;
    for _ in 0..attempts {
        let first = TcpListener::bind(addr).await?;
        let port = first.local_addr()?.port();
        let companion = SocketAddr::new(companion_ip(addr), port);
        match TcpListener::bind(companion).await {
            Ok(second) => return Ok((first, second)),
            Err(e) => {
                last_err = Some(io::Error::new(
                    e.kind(),
                    format!("{companion} is not available ({e}); refusing to serve only half of localhost:{port}"),
                ))
            }
        }
    }
    Err(last_err.unwrap_or_else(|| io::Error::other("bind failed")))
}

/// Constant-time equality (length is not secret).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `Authorization: Bearer <token>` present exactly once and matching.
pub fn bearer_ok(headers: &[(String, String)], token: &str) -> bool {
    let mut values = headers.iter().filter(|(k, _)| k == "authorization").map(|(_, v)| v);
    let (Some(v), None) = (values.next(), values.next()) else {
        return false;
    };
    let Some((scheme, cred)) = v.trim().split_once(' ') else {
        return false;
    };
    scheme.eq_ignore_ascii_case("bearer") && ct_eq(cred.trim().as_bytes(), token.as_bytes())
}

async fn reply(w: &mut OwnedWriteHalf, bytes: &[u8]) -> io::Result<()> {
    w.write_all(bytes).await?;
    w.flush().await?;
    let _ = w.shutdown().await;
    Ok(())
}

/// Local refusal (nothing was sent to the other Mac).
async fn refuse(shared: &Shared, w: &mut OwnedWriteHalf, bytes: &[u8]) -> io::Result<()> {
    shared.stats.lock().unwrap().refused_locally += 1;
    reply(w, bytes).await
}

fn body_kind(head: &RequestHead) -> Result<BodyKind, &'static str> {
    let te = head.header("transfer-encoding");
    let cl = head.header("content-length");
    if head.header_count("content-length") > 1 || head.header_count("transfer-encoding") > 1 {
        return Err("duplicate framing headers");
    }
    match (te, cl) {
        (Some(_), Some(_)) => Err("both Transfer-Encoding and Content-Length"),
        (Some(te), None) if te.eq_ignore_ascii_case("chunked") => Ok(BodyKind::Chunked),
        (Some(_), None) => Err("unsupported Transfer-Encoding"),
        (None, Some(cl)) => {
            // Digits only: `u64::from_str` would accept a leading '+'.
            if cl.is_empty() || !cl.bytes().all(|b| b.is_ascii_digit()) {
                return Err("bad Content-Length");
            }
            let n: u64 = cl.parse().map_err(|_| "bad Content-Length")?;
            Ok(if n == 0 { BodyKind::None } else { BodyKind::Length(n) })
        }
        (None, None) => Ok(BodyKind::None),
    }
}

async fn serve_connection(shared: &Shared, tcp: TcpStream, port: u16) -> io::Result<()> {
    tcp.set_nodelay(true)?;
    let (mut r, mut w) = tcp.into_split();
    let Some((raw_head, early_body)) = tokio::time::timeout(HEAD_TIMEOUT, http::read_head(&mut r))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "request head timeout"))??
    else {
        return Ok(());
    };
    let head = match http::parse_request_head(&raw_head) {
        Ok(h) => h,
        Err(_) => {
            return refuse(shared, &mut w, &http::error_response(400, "bad_request", "Malformed HTTP request.")).await
        }
    };
    if !allowlist::host_allowed(head.header("host"), port) || head.header_count("host") != 1 {
        return refuse(shared, &mut w, &http::forbidden()).await;
    }
    // The app's own window (exact Origin, exactly one Origin header) is the only browser
    // context let through; everything else with an Origin / Sec-Fetch-Site is refused.
    let app = app_origin_of(&head.headers, shared.app_origin.as_deref());
    let cors = app.map(str::to_string);
    if app.is_none() && allowlist::browser_origin_refused(&head.headers) {
        return refuse(shared, &mut w, &http::forbidden()).await;
    }
    if let Some(origin) = app {
        if head.method == "OPTIONS" {
            return refuse(shared, &mut w, &preflight_response(origin)).await;
        }
    }
    if let Some(token) = &shared.token {
        if !bearer_ok(&head.headers, token) {
            shared.stats.lock().unwrap().unauthorized += 1;
            return refuse(shared, &mut w, &with_cors(http::unauthorized(), cors.as_deref())).await;
        }
    }
    let Some(canon) = allowlist::canonical(&head.method, &head.target) else {
        return refuse(shared, &mut w, &with_cors(http::forbidden(), cors.as_deref())).await;
    };
    let kind = match body_kind(&head) {
        Ok(k) => k,
        Err(msg) => {
            return refuse(shared, &mut w, &with_cors(http::error_response(400, "bad_request", msg), cors.as_deref()))
                .await
        }
    };
    if let BodyKind::Length(n) = kind {
        if n > MAX_BODY {
            return refuse(
                shared,
                &mut w,
                &with_cors(
                    http::error_response(413, "body_too_large", "Request body larger than 32 MiB."),
                    cors.as_deref(),
                ),
            )
            .await;
        }
    }
    if head
        .header("expect")
        .is_some_and(|v| v.eq_ignore_ascii_case("100-continue"))
    {
        w.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await?;
        w.flush().await?;
    }
    // M5: a version mismatch (or another reason to stop forwarding) is answered here.
    let blocked = shared.blocked.lock().unwrap().clone();
    if let Some(answer) = blocked {
        shared.stats.lock().unwrap().answered_offline += 1;
        let _ = drain_body(&mut r, &early_body, kind, shared.local_limits.body_idle_timeout).await;
        return refuse(shared, &mut w, &with_cors(answer.bytes(), cors.as_deref())).await;
    }
    // M5: the paired Mac is away (asleep / waking / off the network): wait briefly for the
    // connection, then answer with a clear 503 JSON error instead of a dead port.
    if shared.connection.borrow().is_none() {
        let mut rx = shared.connection.clone();
        let back = tokio::time::timeout(shared.connect_wait, rx.wait_for(|g| g.is_some()))
            .await
            .is_ok_and(|r| r.is_ok());
        if !back {
            let answer = shared.offline.lock().unwrap().clone();
            shared.stats.lock().unwrap().answered_offline += 1;
            let _ = drain_body(&mut r, &early_body, kind, shared.local_limits.body_idle_timeout).await;
            return refuse(shared, &mut w, &with_cors(answer.bytes(), cors.as_deref())).await;
        }
    }
    shared.stats.lock().unwrap().requests += 1;

    let (mut reader, mut writer) = match shared.take_channel().await {
        Ok(c) => c,
        Err(e) => {
            return reply(
                &mut w,
                &with_cors(
                    http::error_response(502, "peer_unreachable", &format!("The other Mac is not reachable ({e}).")),
                    cors.as_deref(),
                ),
            )
            .await
        }
    };
    let req = ReqHead {
        method: canon.method.to_string(),
        target: canon.target(),
        headers: allowlist::filter_request_headers(&head.headers),
        body: kind,
        proto: Some(shared.versions.proto),
    };
    if let Err(e) = writer.send_json(FrameType::ReqHead, &req).await {
        return reply(
            &mut w,
            &with_cors(
                http::error_response(502, "peer_unreachable", &format!("The other Mac is not reachable ({e}).")),
                cors.as_deref(),
            ),
        )
        .await;
    }

    enum End {
        ResponseDone(io::Result<()>),
        ClientGone,
        BodyRefused(&'static str),
    }
    let mut state = RespState {
        cors: cors.clone(),
        ..RespState::default()
    };
    let end = {
        let up = upload_then_watch(&mut r, early_body, kind, &mut writer, shared.local_limits.body_idle_timeout);
        let down = relay_response(&mut reader, &mut w, &mut state);
        tokio::pin!(up, down);
        tokio::select! {
            res = &mut down => End::ResponseDone(res),
            u = &mut up => match u {
                Upload::ClientGone => End::ClientGone,
                Upload::TooLarge => End::BodyRefused("Request body larger than 32 MiB."),
                Upload::Malformed => End::BodyRefused("Malformed chunked request body."),
                Upload::TimedOut => End::BodyRefused("Timed out waiting for the request body."),
            },
        }
    };
    match end {
        End::ResponseDone(Ok(_)) => {
            let _ = writer.finish().await;
            let _ = w.shutdown().await;
        }
        End::ResponseDone(Err(e)) => {
            // Peer lost / stream broken mid-request, or the editor's socket failed.
            let _ = writer.send(FrameType::Cancel, &[]).await;
            if state.head_sent {
                shared.stats.lock().unwrap().peer_lost_mid_request += 1;
                if state.chunked && state.event_stream && state.framer.as_ref().is_some_and(|f| f.ok) {
                    // Only whole chunks (= whole SSE events) reached the editor, so the
                    // documented error event lands on an event boundary; then the stream
                    // ends cleanly (terminating chunk). See [`PEER_LOST_EVENT`].
                    let _ = w.write_all(&peer_lost_tail()).await;
                    let _ = w.flush().await;
                }
            } else {
                let _ = w
                    .write_all(&with_cors(
                        http::error_response(502, "peer_lost", &format!("The other Mac stopped answering ({e}).")),
                        cors.as_deref(),
                    ))
                    .await;
            }
            let _ = w.shutdown().await;
        }
        End::ClientGone => {
            shared.stats.lock().unwrap().cancels_sent += 1;
            let _ = writer.send(FrameType::Cancel, &[]).await;
            if shared.verbose {
                eprintln!("use event=cancel_sent unix_ms={}", unix_ms());
            }
            // Dropping reader/writer resets the stream.
        }
        End::BodyRefused(msg) => {
            let _ = writer.send(FrameType::Cancel, &[]).await;
            if !state.head_sent {
                let status = if msg.contains("32 MiB") {
                    413
                } else if msg.starts_with("Timed out") {
                    408
                } else {
                    400
                };
                let _ = reply(&mut w, &with_cors(http::error_response(status, "bad_request", msg), cors.as_deref())).await;
            }
        }
    }
    Ok(())
}

#[derive(Debug, Default)]
struct RespState {
    head_sent: bool,
    chunked: bool,
    event_stream: bool,
    /// The app window's origin when the request came from it (CORS headers added).
    cors: Option<String>,
    /// Chunked responses: holds back a partial chunk so the editor only ever sees whole
    /// chunks (M5 — a peer lost mid-stream then ends on an event boundary).
    framer: Option<ChunkFramer>,
}

/// The SSE event a stream ends with when the other Mac stops answering mid-stream
/// (`error.slipstream_code = "peer_lost"`), followed by the terminating chunk.
pub const PEER_LOST_EVENT: &str = "peer_lost";

/// `data: {"error":{…"slipstream_code":"peer_lost"…}}\n\n` as one chunk + `0\r\n\r\n`.
pub fn peer_lost_tail() -> Vec<u8> {
    let event = format!(
        "data: {}\n\n",
        String::from_utf8_lossy(&http::error_body(
            502,
            "server_error",
            PEER_LOST_EVENT,
            "The other Mac stopped answering.",
        ))
    );
    format!("{:x}\r\n{event}\r\n0\r\n\r\n", event.len()).into_bytes()
}

/// Splits a raw chunked byte stream at chunk boundaries: [`push`](Self::push) returns the
/// bytes up to the last complete chunk and keeps the rest. Byte-identical overall (only
/// delayed by at most one partial chunk). On malformed framing it gives up (`ok = false`)
/// and passes everything through.
#[derive(Debug)]
pub struct ChunkFramer {
    state: ChunkState,
    line: Vec<u8>,
    pending: Vec<u8>,
    pub ok: bool,
}

impl Default for ChunkFramer {
    fn default() -> Self {
        Self {
            state: ChunkState::Size,
            line: Vec::new(),
            pending: Vec::new(),
            ok: true,
        }
    }
}

impl ChunkFramer {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        if !self.ok {
            return bytes.to_vec();
        }
        let base = self.pending.len();
        self.pending.extend_from_slice(bytes);
        let mut boundary: Option<usize> = None;
        let mut i = base;
        while i < self.pending.len() {
            let b = self.pending[i];
            match self.state {
                ChunkState::Done => break,
                ChunkState::Size | ChunkState::Trailer => {
                    self.line.push(b);
                    if self.line.len() > 1024 {
                        return self.give_up();
                    }
                    if self.line.ends_with(b"\r\n") {
                        let line = String::from_utf8_lossy(&self.line[..self.line.len() - 2]).into_owned();
                        self.line.clear();
                        if self.state == ChunkState::Trailer {
                            if line.is_empty() {
                                self.state = ChunkState::Done;
                                boundary = Some(i + 1);
                            }
                        } else {
                            match u64::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16) {
                                Ok(0) => self.state = ChunkState::Trailer,
                                Ok(n) => self.state = ChunkState::Data(n),
                                Err(_) => return self.give_up(),
                            }
                        }
                    }
                    i += 1;
                }
                ChunkState::Data(left) => {
                    let n = (left as usize).min(self.pending.len() - i);
                    i += n;
                    let left = left - n as u64;
                    self.state = if left == 0 { ChunkState::DataCrlf(0) } else { ChunkState::Data(left) };
                }
                ChunkState::DataCrlf(seen) => {
                    let expect = if seen == 0 { b'\r' } else { b'\n' };
                    if b != expect {
                        return self.give_up();
                    }
                    i += 1;
                    if seen == 0 {
                        self.state = ChunkState::DataCrlf(1);
                    } else {
                        self.state = ChunkState::Size;
                        boundary = Some(i);
                    }
                }
            }
        }
        if self.state == ChunkState::Done {
            // anything after the last chunk is passed through as is
            return std::mem::take(&mut self.pending);
        }
        match boundary {
            Some(k) => {
                let rest = self.pending.split_off(k);
                std::mem::replace(&mut self.pending, rest)
            }
            None => Vec::new(),
        }
    }

    fn give_up(&mut self) -> Vec<u8> {
        self.ok = false;
        std::mem::take(&mut self.pending)
    }
}

/// `Some(origin)` when the request carries exactly one `Origin` header equal to the
/// configured app origin.
fn app_origin_of<'a>(headers: &[(String, String)], app: Option<&'a str>) -> Option<&'a str> {
    let app = app?;
    let mut origins = headers.iter().filter(|(k, _)| k == "origin").map(|(_, v)| v.trim());
    match (origins.next(), origins.next()) {
        (Some(o), None) if o == app => Some(app),
        _ => None,
    }
}

/// Local answer to the app window's CORS preflight (never forwarded, no token needed:
/// browsers send preflights without credentials).
fn preflight_response(origin: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: {origin}\r\n\
         Access-Control-Allow-Methods: GET, POST\r\n\
         Access-Control-Allow-Headers: authorization, content-type\r\n\
         Access-Control-Max-Age: 600\r\nVary: Origin\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .into_bytes()
}

/// Add `Access-Control-Allow-Origin: <origin>` + `Vary: Origin` to a response (head or a
/// complete response), dropping any `Access-Control-*` header the other side sent.
/// Unchanged when `origin` is `None` or the bytes hold no complete head.
pub fn with_cors(bytes: Vec<u8>, origin: Option<&str>) -> Vec<u8> {
    let Some(origin) = origin else { return bytes };
    let Some(end) = http::find_head_end(&bytes) else { return bytes };
    let head = String::from_utf8_lossy(&bytes[..end]);
    let mut lines: Vec<&str> = head.split("\r\n").filter(|l| !l.is_empty()).collect();
    lines.retain(|l| {
        let name = l.split(':').next().unwrap_or("").trim().to_ascii_lowercase();
        !(name.starts_with("access-control-") || name == "vary")
    });
    let mut out = lines.join("\r\n");
    out.push_str(&format!("\r\nAccess-Control-Allow-Origin: {origin}\r\nVary: Origin\r\n\r\n"));
    let mut v = out.into_bytes();
    v.extend_from_slice(&bytes[end..]);
    v
}

enum Upload {
    ClientGone,
    TooLarge,
    Malformed,
    TimedOut,
}

/// One body read with the idle timeout. `Err(Upload)` ends the upload.
async fn read_body(r: &mut OwnedReadHalf, buf: &mut [u8], idle: Duration) -> Result<usize, Upload> {
    match tokio::time::timeout(idle, r.read(buf)).await {
        Err(_) => Err(Upload::TimedOut),
        Ok(Ok(0)) | Ok(Err(_)) => Err(Upload::ClientGone),
        Ok(Ok(k)) => Ok(k),
    }
}

/// Stream the request body as ReqBody frames (cap 32 MiB), send the end marker, then
/// watch the editor's socket: EOF/error = the editor went away (stop button).
async fn upload_then_watch(
    r: &mut OwnedReadHalf,
    early: Vec<u8>,
    kind: BodyKind,
    writer: &mut ChannelWriter,
    idle: Duration,
) -> Upload {
    let mut buf = vec![0u8; BODY_CHUNK];
    match kind {
        BodyKind::None => {}
        BodyKind::Length(n) => {
            let mut left = n;
            let first = early.len().min(n as usize);
            if writer.send_body(FrameType::ReqBody, &early[..first]).await.is_err() {
                return pending_forever().await;
            }
            left -= first as u64;
            while left > 0 {
                let want = (left as usize).min(buf.len());
                match read_body(r, &mut buf[..want], idle).await {
                    Err(u) => return u,
                    Ok(k) => {
                        left -= k as u64;
                        if writer.send(FrameType::ReqBody, &buf[..k]).await.is_err() {
                            return pending_forever().await;
                        }
                    }
                }
            }
        }
        BodyKind::Chunked => {
            let mut dec = ChunkedDecoder::default();
            let mut pending = early;
            loop {
                let mut out = Vec::new();
                match dec.feed(&pending, &mut out) {
                    Err(()) => return Upload::Malformed,
                    Ok(done) => {
                        if dec.total > MAX_BODY {
                            return Upload::TooLarge;
                        }
                        if writer.send_body(FrameType::ReqBody, &out).await.is_err() {
                            return pending_forever().await;
                        }
                        if done {
                            break;
                        }
                    }
                }
                match read_body(r, &mut buf, idle).await {
                    Err(u) => return u,
                    Ok(k) => pending = buf[..k].to_vec(),
                }
            }
        }
    }
    if writer.send(FrameType::ReqBody, &[]).await.is_err() {
        return pending_forever().await;
    }
    // Watch for the editor going away. Bytes after the request (a pipelined request)
    // are ignored: every response is `Connection: close`.
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => return Upload::ClientGone,
            Ok(_) => {}
        }
    }
}

/// Read (and discard) the rest of a request body before a local answer, so closing the
/// socket does not reset the connection under the editor (bounded: 32 MiB, idle timeout).
async fn drain_body(r: &mut OwnedReadHalf, early: &[u8], kind: BodyKind, idle: Duration) -> Result<(), Upload> {
    let mut buf = vec![0u8; BODY_CHUNK];
    match kind {
        BodyKind::None => Ok(()),
        BodyKind::Length(n) => {
            let mut left = n.saturating_sub(early.len() as u64);
            while left > 0 {
                let want = (left as usize).min(buf.len());
                let k = read_body(r, &mut buf[..want], idle).await?;
                left -= k as u64;
            }
            Ok(())
        }
        BodyKind::Chunked => {
            let mut dec = ChunkedDecoder::default();
            let mut sink = Vec::new();
            let mut pending = early.to_vec();
            loop {
                sink.clear();
                match dec.feed(&pending, &mut sink) {
                    Err(()) => return Err(Upload::Malformed),
                    Ok(true) => return Ok(()),
                    Ok(false) if dec.total > MAX_BODY => return Err(Upload::TooLarge),
                    Ok(false) => {}
                }
                let k = read_body(r, &mut buf, idle).await?;
                pending = buf[..k].to_vec();
            }
        }
    }
}

/// A broken stream write is reported by the response side; keep this branch quiet.
async fn pending_forever() -> Upload {
    std::future::pending().await
}

/// Response frames → the editor's socket, flushing every frame.
async fn relay_response(
    reader: &mut ChannelReader,
    w: &mut OwnedWriteHalf,
    state: &mut RespState,
) -> io::Result<()> {
    loop {
        let frame = reader
            .next()
            .await?
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "stream ended before the response"))?;
        match frame.kind {
            FrameType::RespHead if !state.head_sent => {
                if let Ok(h) = http::parse_response_head(&frame.payload) {
                    state.chunked = h.is_chunked();
                    state.event_stream = h.is_event_stream();
                    if state.chunked {
                        state.framer = Some(ChunkFramer::default());
                    }
                }
                let head = with_cors(frame.payload, state.cors.as_deref());
                w.write_all(&head).await?;
                w.flush().await?;
                state.head_sent = true;
            }
            FrameType::RespBody if state.head_sent => {
                if frame.payload.is_empty() {
                    if let Some(f) = state.framer.as_mut() {
                        // clean end: whatever is held back goes out unchanged
                        let rest = std::mem::take(&mut f.pending);
                        if !rest.is_empty() {
                            w.write_all(&rest).await?;
                            w.flush().await?;
                        }
                    }
                    return Ok(());
                }
                let out = match state.framer.as_mut() {
                    Some(f) => f.push(&frame.payload),
                    None => frame.payload,
                };
                if !out.is_empty() {
                    w.write_all(&out).await?;
                    w.flush().await?;
                }
            }
            FrameType::Error if !state.head_sent => {
                let err: WireError = serde_json::from_slice(&frame.payload)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                w.write_all(&with_cors(
                    http::error_response(err.status, &err.code, &err.message),
                    state.cors.as_deref(),
                ))
                .await?;
                w.flush().await?;
                state.head_sent = true;
                return Ok(());
            }
            _ => {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected frame"));
            }
        }
    }
}

/// Incremental HTTP/1.1 chunked decoder for request bodies (trailers ignored).
#[derive(Default)]
struct ChunkedDecoder {
    state: ChunkState,
    line: Vec<u8>,
    total: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum ChunkState {
    #[default]
    Size,
    Data(u64),
    DataCrlf(u8),
    Trailer,
    Done,
}

impl ChunkedDecoder {
    /// Feed bytes; decoded data is appended to `out`. Returns Ok(true) when done.
    fn feed(&mut self, mut input: &[u8], out: &mut Vec<u8>) -> Result<bool, ()> {
        while !input.is_empty() {
            match self.state {
                ChunkState::Done => return Ok(true),
                ChunkState::Size | ChunkState::Trailer => {
                    let b = input[0];
                    input = &input[1..];
                    self.line.push(b);
                    if self.line.len() > 1024 {
                        return Err(());
                    }
                    if self.line.ends_with(b"\r\n") {
                        let line = std::str::from_utf8(&self.line[..self.line.len() - 2]).map_err(|_| ())?;
                        if self.state == ChunkState::Trailer {
                            if line.is_empty() {
                                self.state = ChunkState::Done;
                                return Ok(true);
                            }
                        } else {
                            let size = u64::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
                                .map_err(|_| ())?;
                            self.state = if size == 0 {
                                ChunkState::Trailer
                            } else {
                                ChunkState::Data(size)
                            };
                        }
                        self.line.clear();
                    }
                }
                ChunkState::Data(left) => {
                    let n = (left as usize).min(input.len());
                    out.extend_from_slice(&input[..n]);
                    self.total += n as u64;
                    if self.total > MAX_BODY {
                        return Ok(false);
                    }
                    input = &input[n..];
                    let left = left - n as u64;
                    self.state = if left == 0 {
                        ChunkState::DataCrlf(0)
                    } else {
                        ChunkState::Data(left)
                    };
                }
                ChunkState::DataCrlf(seen) => {
                    let expect = if seen == 0 { b'\r' } else { b'\n' };
                    if input[0] != expect {
                        return Err(());
                    }
                    input = &input[1..];
                    self.state = if seen == 0 {
                        ChunkState::DataCrlf(1)
                    } else {
                        ChunkState::Size
                    };
                }
            }
        }
        Ok(self.state == ChunkState::Done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            cases: std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(2_000),
            failure_persistence: None,
            ..Default::default()
        })]

        /// v0.6 review §8: the editor-side chunked request decoder takes arbitrary bytes in
        /// arbitrary splits without panicking and never yields more than it was fed.
        #[test]
        fn chunked_decoder_total(buf in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..512), cut in 0usize..512) {
            let cut = cut.min(buf.len());
            let mut d = ChunkedDecoder::default();
            let mut out = Vec::new();
            let r1 = d.feed(&buf[..cut], &mut out);
            if r1 == Ok(false) {
                let _ = d.feed(&buf[cut..], &mut out);
            }
            proptest::prop_assert!(out.len() <= buf.len());
        }
    }

    #[test]
    fn bearer_token_check() {
        let h = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
        };
        let t = "0123456789abcdef0123456789abcdef";
        assert!(bearer_ok(&h(&[("authorization", &format!("Bearer {t}"))]), t));
        assert!(bearer_ok(&h(&[("authorization", &format!("bearer {t}"))]), t));
        assert!(!bearer_ok(&h(&[]), t));
        assert!(!bearer_ok(&h(&[("authorization", t)]), t));
        assert!(!bearer_ok(&h(&[("authorization", &format!("Basic {t}"))]), t));
        assert!(!bearer_ok(&h(&[("authorization", "Bearer 0123456789abcdef0123456789abcdeX")]), t));
        assert!(!bearer_ok(&h(&[("authorization", "Bearer 0123")]), t));
        // two Authorization headers: refused even if one matches
        assert!(!bearer_ok(
            &h(&[("authorization", "Bearer nope"), ("authorization", &format!("Bearer {t}"))]),
            t
        ));
        assert!(ct_eq(b"abc", b"abc") && !ct_eq(b"abc", b"abd") && !ct_eq(b"abc", b"ab"));
    }

    #[test]
    fn chunk_framer_emits_whole_chunks_only_and_is_byte_identical() {
        let stream = b"5\r\nhello\r\nf\r\ndata: {\"x\":1}\n\n\r\n0\r\n\r\n".to_vec();
        for split in 0..=stream.len() {
            for split2 in split..=stream.len() {
                let mut f = ChunkFramer::default();
                let mut out = Vec::new();
                for part in [&stream[..split], &stream[split..split2], &stream[split2..]] {
                    let got = f.push(part);
                    out.extend_from_slice(&got);
                    // every emitted prefix ends on a chunk boundary
                    let pos = out.len();
                    assert!([0, 10, 30, stream.len()].contains(&pos), "split {split}/{split2}: {pos}");
                }
                assert!(f.ok);
                assert_eq!(out, stream);
            }
        }
        let mut f = ChunkFramer::default();
        assert_eq!(f.push(b"zz\r\nabc"), b"zz\r\nabc".to_vec());
        assert!(!f.ok);
        assert_eq!(f.push(b"more"), b"more".to_vec());
        let tail = String::from_utf8(peer_lost_tail()).unwrap();
        assert!(tail.contains("\"slipstream_code\":\"peer_lost\"") && tail.ends_with("\r\n0\r\n\r\n"), "{tail}");
    }

    #[test]
    fn chunked_decoder_incremental() {
        let body = b"4\r\nWiki\r\n5;ext=1\r\npedia\r\n0\r\nX-Trailer: 1\r\n\r\n";
        for split in 0..body.len() {
            let mut d = ChunkedDecoder::default();
            let mut out = Vec::new();
            let a = d.feed(&body[..split], &mut out).unwrap();
            let b = d.feed(&body[split..], &mut out).unwrap();
            assert!(b || a);
            assert_eq!(out, b"Wikipedia");
        }
        let mut d = ChunkedDecoder::default();
        assert!(d.feed(b"4\r\nWikiXX", &mut Vec::new()).is_err());
        let mut d = ChunkedDecoder::default();
        assert!(d.feed(b"zz\r\n", &mut Vec::new()).is_err());
    }

    #[test]
    fn body_framing_rules() {
        let head = |h: &[(&str, &str)]| RequestHead {
            method: "POST".into(),
            target: "/v1/completions".into(),
            headers: h.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        };
        assert_eq!(body_kind(&head(&[])), Ok(BodyKind::None));
        assert_eq!(body_kind(&head(&[("content-length", "5")])), Ok(BodyKind::Length(5)));
        assert_eq!(body_kind(&head(&[("transfer-encoding", "chunked")])), Ok(BodyKind::Chunked));
        assert!(body_kind(&head(&[("transfer-encoding", "chunked"), ("content-length", "5")])).is_err());
        assert!(body_kind(&head(&[("content-length", "5"), ("content-length", "6")])).is_err());
        assert!(body_kind(&head(&[("transfer-encoding", "gzip")])).is_err());
        assert!(body_kind(&head(&[("content-length", "-1")])).is_err());
        // M1 review: only digits (u64::from_str alone accepts "+5")
        for bad in ["+5", " 5", "5 ", "0x5", "5,5", "", "1e3", "٥"] {
            assert!(body_kind(&head(&[("content-length", bad)])).is_err(), "{bad:?}");
        }
        assert_eq!(body_kind(&head(&[("content-length", "007")])), Ok(BodyKind::Length(7)));
    }
}
