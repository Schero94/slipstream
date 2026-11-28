//! `/slipstream/pair/1` — pairing two of your own Macs with a 6-digit code (v0.6 M2).
//!
//! docs/plans/2026-11-28-use-your-other-mac.md §2. Crypto: `p2p_crypto::pake` (SPAKE2,
//! Ed25519 group, idA/idB = PeerIds, HMAC key confirmation over both PeerIds, both static
//! X25519 keys, name, version and capability).
//!
//! # Wire (each message `[u32 BE len][JSON]`, ≤ 4 KiB)
//!
//! ```text
//! client (A, types the code)                  server (B, shows the code)
//!   Start  {spake_a, x25519_a}          →
//!                                       ←     Reply {spake_b, x25519_b, info_b*, confirm_b}
//!   Finish {info_a*, confirm_a}         →                  (* = encrypted under K)
//!                                       ←     Done {ack*}   (B stored A)
//!   A stores B
//! ```
//! `Refused {code, message}` from B ends a session early (window closed / expired / busy /
//! wrong code). A second stream type on the same protocol is `Unpair`: hello, then the
//! authenticated SealedStream (pinned static keys) carrying one `Unpair` frame.
//!
//! # Window (server)
//! [`PairingWindow`]: code from OsRng, open for [`PAIRING_WINDOW`] (5 min, injectable
//! [`Clock`]), one session at a time, closes after one success or after
//! [`MAX_FAILED_ATTEMPTS`] failed sessions. A session counts as an attempt as soon as B
//! has sent its `Reply` (that is when an attacker could test one guess), so aborted
//! sessions count too. Untrusted peers are admitted by the gate only while the window is
//! open, and only this protocol serves them.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use libp2p::multiaddr::Protocol;
use libp2p::swarm::SwarmEvent;
use libp2p::{Multiaddr, PeerId};
use p2p_crypto::{NodeKeypair, PairInfo, PairingCode, PakeError, PakeRole, PakeStart, StreamRole, Transcript};
use p2p_forward::proto::{channel_context, open_channel, FrameType};
use p2p_forward::trust::PeerTrust;
use p2p_libp2p::paired::{build_paired_swarm, Control, PairedConfig, PAIR_PROTOCOL_V1};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::task::JoinHandle;
use tokio_util::compat::FuturesAsyncReadCompatExt;

use crate::trust::{unix_now, NodeTrust, PeerRole, TrustStore, TrustedPeer};

pub const PAIRING_WINDOW: Duration = Duration::from_secs(5 * 60);
pub const MAX_FAILED_ATTEMPTS: u32 = 3;
pub const MAX_MSG: usize = 4096;
/// Per-message timeout inside a session (a human already typed the code before dialing).
pub const MSG_TIMEOUT: Duration = Duration::from_secs(10);
pub const DIAL_TIMEOUT: Duration = Duration::from_secs(5);
const NAME_MAX: usize = 64;

// ---------------------------------------------------------------- clock + window

pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> Instant;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Test clock: `now = base + offset`, advanced by hand.
pub struct ManualClock {
    base: Instant,
    offset: Mutex<Duration>,
}

impl Default for ManualClock {
    fn default() -> Self {
        Self {
            base: Instant::now(),
            offset: Mutex::new(Duration::ZERO),
        }
    }
}

impl ManualClock {
    pub fn advance(&self, by: Duration) {
        *self.offset.lock().unwrap() += by;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Instant {
        self.base + *self.offset.lock().unwrap()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowState {
    Open,
    Paired,
    TooManyFailures,
    Expired,
    Cancelled,
}

struct WindowInner {
    failed: u32,
    busy: bool,
    closed: Option<WindowState>,
    paired: Option<TrustedPeer>,
}

pub struct PairingWindow {
    code: PairingCode,
    opened: Instant,
    ttl: Duration,
    clock: Arc<dyn Clock>,
    inner: Mutex<WindowInner>,
    epoch: p2p_forward::TrustEpoch,
}

/// Why the server refused a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: &'static str,
    pub message: &'static str,
}

impl PairingWindow {
    /// A new window with a fresh random code.
    pub fn open(clock: Arc<dyn Clock>, epoch: p2p_forward::TrustEpoch) -> Arc<Self> {
        Self::open_with_code(PairingCode::generate(), clock, epoch)
    }

    pub fn open_with_code(code: PairingCode, clock: Arc<dyn Clock>, epoch: p2p_forward::TrustEpoch) -> Arc<Self> {
        let w = Arc::new(Self {
            code,
            opened: clock.now(),
            ttl: PAIRING_WINDOW,
            clock,
            inner: Mutex::new(WindowInner {
                failed: 0,
                busy: false,
                closed: None,
                paired: None,
            }),
            epoch,
        });
        w.epoch.bump();
        w
    }

    pub fn code(&self) -> &PairingCode {
        &self.code
    }

    pub fn remaining(&self) -> Duration {
        (self.opened + self.ttl).saturating_duration_since(self.clock.now())
    }

    pub fn state(&self) -> WindowState {
        let inner = self.inner.lock().unwrap();
        if let Some(s) = inner.closed {
            return s;
        }
        if self.clock.now() >= self.opened + self.ttl {
            return WindowState::Expired;
        }
        WindowState::Open
    }

    pub fn is_open(&self) -> bool {
        self.state() == WindowState::Open
    }

    pub fn failed_attempts(&self) -> u32 {
        self.inner.lock().unwrap().failed
    }

    pub fn paired_peer(&self) -> Option<TrustedPeer> {
        self.inner.lock().unwrap().paired.clone()
    }

    pub fn cancel(&self) {
        let mut inner = self.inner.lock().unwrap();
        if inner.closed.is_none() {
            inner.closed = Some(WindowState::Cancelled);
        }
        drop(inner);
        self.epoch.bump();
    }

    fn refusal_for(state: WindowState) -> Refusal {
        match state {
            WindowState::Expired => Refusal {
                code: "expired",
                message: "The pairing code expired. Show a new code on the other Mac.",
            },
            WindowState::TooManyFailures => Refusal {
                code: "too_many_attempts",
                message: "Too many wrong codes. Show a new code on the other Mac.",
            },
            WindowState::Paired => Refusal {
                code: "already_used",
                message: "This code was already used. Show a new code on the other Mac.",
            },
            WindowState::Cancelled | WindowState::Open => Refusal {
                code: "not_pairing",
                message: "The other Mac is not showing a pairing code.",
            },
        }
    }

    /// Start a session (one at a time).
    pub fn begin(self: &Arc<Self>) -> Result<Attempt, Refusal> {
        let state = self.state();
        if state != WindowState::Open {
            return Err(Self::refusal_for(state));
        }
        let mut inner = self.inner.lock().unwrap();
        if inner.busy {
            return Err(Refusal {
                code: "busy",
                message: "Another Mac is pairing right now. Try again in a moment.",
            });
        }
        inner.busy = true;
        Ok(Attempt {
            window: self.clone(),
            armed: false,
            done: false,
        })
    }

    /// Wait until the window is no longer open.
    pub async fn wait_closed(&self) -> WindowState {
        loop {
            let s = self.state();
            if s != WindowState::Open {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// One pairing session on the server; a dropped, armed, unsuccessful attempt counts as a
/// failure.
pub struct Attempt {
    window: Arc<PairingWindow>,
    armed: bool,
    done: bool,
}

impl Attempt {
    /// B has sent its `Reply` — from now on this session counts.
    pub fn arm(&mut self) {
        self.armed = true;
    }

    pub fn succeed(mut self, peer: TrustedPeer) {
        self.done = true;
        let mut inner = self.window.inner.lock().unwrap();
        inner.closed = Some(WindowState::Paired);
        inner.paired = Some(peer);
    }
}

impl Drop for Attempt {
    fn drop(&mut self) {
        let mut inner = self.window.inner.lock().unwrap();
        inner.busy = false;
        if self.armed && !self.done {
            inner.failed += 1;
            if inner.failed >= MAX_FAILED_ATTEMPTS && inner.closed.is_none() {
                inner.closed = Some(WindowState::TooManyFailures);
            }
        }
        drop(inner);
        self.window.epoch.bump();
    }
}

// ---------------------------------------------------------------- wire

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum PairMsg {
    Start { spake: String, x25519: String },
    Reply { spake: String, x25519: String, info: String, confirm: String },
    Finish { info: String, confirm: String },
    Done { ack: String },
    Refused { code: String, message: String },
    Unpair,
}

pub async fn write_msg<W: AsyncWrite + Unpin>(w: &mut W, msg: &PairMsg) -> io::Result<()> {
    let bytes = serde_json::to_vec(msg).map_err(io::Error::other)?;
    if bytes.len() > MAX_MSG {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "pair message too large"));
    }
    w.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    w.write_all(&bytes).await?;
    w.flush().await
}

pub async fn read_msg<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<PairMsg> {
    let read = async {
        let mut len = [0u8; 4];
        r.read_exact(&mut len).await?;
        let len = u32::from_be_bytes(len) as usize;
        if len > MAX_MSG {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "pair message too large"));
        }
        let mut buf = vec![0u8; len];
        r.read_exact(&mut buf).await?;
        serde_json::from_slice(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    };
    tokio::time::timeout(MSG_TIMEOUT, read)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "pairing message timed out"))?
}

fn unhex<const N: usize>(s: &str) -> io::Result<[u8; N]> {
    hex::decode(s)
        .ok()
        .and_then(|v| v.try_into().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad hex field"))
}

fn unhex_vec(s: &str) -> io::Result<Vec<u8>> {
    hex::decode(s).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad hex field"))
}

/// The Ed25519 public key inside an Ed25519 PeerId (identity multihash), hex.
pub fn identity_hex(peer: &PeerId) -> String {
    let mh: &libp2p::multihash::Multihash<64> = peer.as_ref();
    if mh.code() == 0 {
        if let Ok(pk) = libp2p::identity::PublicKey::try_decode_protobuf(mh.digest()) {
            if let Ok(ed) = pk.try_into_ed25519() {
                return hex::encode(ed.to_bytes());
            }
        }
    }
    String::new()
}

/// Display name as stored: control characters dropped, ≤ 64 characters.
pub fn clean_name(name: &str) -> String {
    let s: String = p2p_forward::allowlist::display_safe(name).chars().take(NAME_MAX).collect();
    let s = s.trim().to_string();
    if s.is_empty() {
        "Mac".into()
    } else {
        s
    }
}

// ---------------------------------------------------------------- errors

#[derive(Debug, thiserror::Error)]
pub enum PairError {
    #[error("could not reach the other Mac: {0}")]
    Unreachable(String),
    #[error("{message}")]
    Refused { code: String, message: String },
    #[error("pairing failed: wrong code (or someone is in between). Check the code on the other Mac.")]
    WrongCode,
    #[error("pairing protocol error: {0}")]
    Protocol(String),
    #[error(transparent)]
    Io(io::Error),
}

impl From<io::Error> for PairError {
    fn from(e: io::Error) -> Self {
        use io::ErrorKind::*;
        match e.kind() {
            // The gate closes guests as soon as no pairing window is open.
            NotConnected | ConnectionReset | ConnectionAborted | UnexpectedEof | BrokenPipe => PairError::Refused {
                code: "connection_closed".into(),
                message: "The other Mac closed the connection (it is not showing a pairing code, or the code expired)."
                    .into(),
            },
            _ => PairError::Io(e),
        }
    }
}

impl From<PakeError> for PairError {
    fn from(e: PakeError) -> Self {
        match e {
            PakeError::Confirmation => PairError::WrongCode,
            other => PairError::Protocol(other.to_string()),
        }
    }
}

// ---------------------------------------------------------------- server side

/// What the pair-protocol handler needs on either Mac.
pub struct PairCtx {
    pub trust: Arc<NodeTrust>,
    /// Our claims (name, version, "paired Macs may start/stop my model").
    pub info: PairInfo,
    pub stats: Mutex<PairStats>,
}

#[derive(Debug, Default, Clone)]
pub struct PairStats {
    pub sessions: u64,
    pub paired: u64,
    pub failed: u64,
    pub refused: u64,
    pub unpaired: u64,
}

impl PairCtx {
    pub fn new(trust: Arc<NodeTrust>, info: PairInfo) -> Arc<Self> {
        Arc::new(Self {
            trust,
            info,
            stats: Mutex::default(),
        })
    }
}

/// Register `/slipstream/pair/1` on a swarm (share side, use side, pairing listener).
pub fn attach_pair_protocol(mut control: Control, ctx: Arc<PairCtx>) -> io::Result<JoinHandle<()>> {
    let mut incoming = control
        .accept(PAIR_PROTOCOL_V1)
        .map_err(|e| io::Error::other(e.to_string()))?;
    Ok(tokio::spawn(async move {
        while let Some((peer, stream)) = incoming.next().await {
            let ctx = ctx.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_pair_stream(&ctx, peer, stream).await {
                    eprintln!("pair event=session_error peer={peer} error={e}");
                }
            });
        }
    }))
}

async fn refuse<W: AsyncWrite + Unpin>(w: &mut W, r: Refusal) -> io::Result<()> {
    write_msg(
        w,
        &PairMsg::Refused {
            code: r.code.into(),
            message: r.message.into(),
        },
    )
    .await
}

async fn handle_pair_stream(ctx: &PairCtx, peer: PeerId, stream: libp2p::Stream) -> io::Result<()> {
    let mut io = stream.compat();
    let first = read_msg(&mut io).await?;
    match first {
        PairMsg::Unpair => handle_unpair(ctx, peer, io.into_inner()).await,
        PairMsg::Start { spake, x25519 } => {
            ctx.stats.lock().unwrap().sessions += 1;
            let Some(window) = ctx.trust.window() else {
                ctx.stats.lock().unwrap().refused += 1;
                return refuse(&mut io, PairingWindow::refusal_for(WindowState::Cancelled)).await;
            };
            let mut attempt = match window.begin() {
                Ok(a) => a,
                Err(r) => {
                    ctx.stats.lock().unwrap().refused += 1;
                    return refuse(&mut io, r).await;
                }
            };
            let spake_a = unhex_vec(&spake)?;
            let x25519_a: [u8; 32] = unhex(&x25519)?;
            let local = ctx.trust.local_peer();
            let (id_a, id_b) = (peer.to_bytes(), local.to_bytes());
            let (start, spake_b) = PakeStart::new(PakeRole::Server, window.code(), &id_a, &id_b);
            let key = start
                .finish(&spake_a)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            let x25519_b = ctx.trust.node().public_bytes();
            let t = Transcript {
                peer_id_a: &id_a,
                peer_id_b: &id_b,
                x25519_a,
                x25519_b,
            };
            let confirm_b = key.confirm(&t, &ctx.info, &[]);
            write_msg(
                &mut io,
                &PairMsg::Reply {
                    spake: hex::encode(&spake_b),
                    x25519: hex::encode(x25519_b),
                    info: hex::encode(key.seal_info(&ctx.info)),
                    confirm: hex::encode(confirm_b),
                },
            )
            .await?;
            attempt.arm();
            let msg = read_msg(&mut io).await?;
            let PairMsg::Finish { info, confirm } = msg else {
                ctx.stats.lock().unwrap().failed += 1;
                return Err(io::Error::new(io::ErrorKind::InvalidData, "expected Finish"));
            };
            let verified = key
                .open_info(&unhex_vec(&info)?)
                .and_then(|info_a| {
                    key.verify_peer(&t, &info_a, &confirm_b, &unhex_vec(&confirm).unwrap_or_default())
                        .map(|_| info_a)
                });
            let info_a = match verified {
                Ok(i) => i,
                Err(_) => {
                    ctx.stats.lock().unwrap().failed += 1;
                    eprintln!("pair event=wrong_code peer={peer} failed_attempts={}", window.failed_attempts() + 1);
                    drop(attempt); // counts the failure (and may close the window)
                    return refuse(
                        &mut io,
                        Refusal {
                            code: "wrong_code",
                            message: "Wrong code.",
                        },
                    )
                    .await;
                }
            };
            if !window.is_open() {
                // expired while the client was typing / in flight
                return refuse(&mut io, PairingWindow::refusal_for(window.state())).await;
            }
            let entry = TrustedPeer {
                peer_id: peer.to_string(),
                identity_hex: identity_hex(&peer),
                encryption_node_id: hex::encode(x25519_a),
                name: clean_name(&info_a.name),
                role: PeerRole::Use,
                paired_at: unix_now(),
                last_addrs: vec![],
                last_seen: Some(unix_now()),
                app_version: clean_name(&info_a.app_version),
                // we decide what a paired Mac may do with our model
                may_start_model: ctx.info.may_start_model,
                wake_macs: vec![],
            };
            ctx.trust.store.add(entry.clone())?;
            write_msg(&mut io, &PairMsg::Done { ack: hex::encode(key.seal_ack()) }).await?;
            let _ = io.shutdown().await;
            ctx.stats.lock().unwrap().paired += 1;
            eprintln!("pair event=paired peer={peer} name={:?}", entry.name);
            attempt.succeed(entry);
            Ok(())
        }
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected first pairing message")),
    }
}

async fn handle_unpair(ctx: &PairCtx, peer: PeerId, stream: libp2p::Stream) -> io::Result<()> {
    // Only a paired Mac can unpair, proven by the authenticated SealedStream.
    let Some(key) = ctx.trust.trusted_key(&peer) else {
        return Ok(());
    };
    let local = ctx.trust.local_peer();
    let context = channel_context(&peer, &local, &PAIR_PROTOCOL_V1);
    let (mut reader, _writer) = tokio::time::timeout(
        MSG_TIMEOUT,
        open_channel(stream, StreamRole::Responder, &context, ctx.trust.node(), &key),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "unpair handshake timed out"))??;
    let frame = tokio::time::timeout(MSG_TIMEOUT, reader.next())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "unpair timed out"))??;
    if frame.is_some_and(|f| f.kind == FrameType::Unpair) {
        if let Some(removed) = ctx.trust.store.remove_peer(&peer)? {
            ctx.stats.lock().unwrap().unpaired += 1;
            eprintln!("pair event=unpaired_by_peer peer={peer} name={:?}", removed.name);
        }
    }
    Ok(())
}

/// Best-effort `Unpair` to a peer we just removed (uses the key it had).
pub async fn send_unpair(
    mut control: Control,
    node: &NodeKeypair,
    local: PeerId,
    peer: PeerId,
    peer_key: [u8; 32],
) -> io::Result<()> {
    let stream = tokio::time::timeout(Duration::from_millis(300), control.open_stream(peer, PAIR_PROTOCOL_V1))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "open timed out"))?
        .map_err(|e| io::Error::other(e.to_string()))?;
    let mut io = stream.compat();
    write_msg(&mut io, &PairMsg::Unpair).await?;
    let context = channel_context(&local, &peer, &PAIR_PROTOCOL_V1);
    let (_reader, mut writer) = open_channel(io.into_inner(), StreamRole::Initiator, &context, node, &peer_key).await?;
    writer.send(FrameType::Unpair, &[]).await?;
    writer.finish().await
}

/// A listening paired swarm that serves only `/slipstream/pair/1` while `window` is open
/// (`slipstream-node pair --show-code`).
pub struct PairListener {
    pub peer_id: PeerId,
    pub listen_addr: Multiaddr,
    pub window: Arc<PairingWindow>,
    pub ctx: Arc<PairCtx>,
    tasks: Vec<JoinHandle<()>>,
}

impl PairListener {
    pub fn dial_addr(&self) -> Multiaddr {
        self.listen_addr.clone().with(Protocol::P2p(self.peer_id))
    }
}

impl Drop for PairListener {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

pub async fn start_pair_listener(
    trust: Arc<NodeTrust>,
    listen: Multiaddr,
    paired: &PairedConfig,
    info: PairInfo,
    window: Arc<PairingWindow>,
) -> io::Result<PairListener> {
    trust.set_window(Some(window.clone()));
    let identity = p2p_forward::transport_identity(trust.node());
    let gated = p2p_forward::gate::start_gated_listener(
        identity,
        listen,
        paired,
        trust.clone(),
        Default::default(),
        "pair",
        false,
    )
    .await?;
    let ctx = PairCtx::new(trust, info);
    let accept = attach_pair_protocol(gated.control.clone(), ctx.clone())?;
    Ok(PairListener {
        peer_id: gated.peer_id,
        listen_addr: gated.listen_addr,
        window,
        ctx,
        tasks: vec![gated.task, accept],
    })
}

// ---------------------------------------------------------------- client side

/// A connected client swarm (driven in the background) for one pairing session.
pub struct PairConnection {
    pub remote: PeerId,
    pub local: PeerId,
    pub control: Control,
    task: JoinHandle<()>,
}

impl Drop for PairConnection {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Dial `addr` (with or without `/p2p/<id>`; with it, the QUIC TLS check pins the id).
pub async fn connect(node: &NodeKeypair, addr: &Multiaddr, paired: &PairedConfig) -> Result<PairConnection, PairError> {
    let mut swarm = build_paired_swarm(p2p_forward::transport_identity(node), paired);
    let local = *swarm.local_peer_id();
    swarm
        .dial(addr.clone())
        .map_err(|e| PairError::Unreachable(e.to_string()))?;
    let remote = tokio::time::timeout(DIAL_TIMEOUT, async {
        loop {
            match swarm.select_next_some().await {
                SwarmEvent::ConnectionEstablished { peer_id, .. } => return Ok(peer_id),
                SwarmEvent::OutgoingConnectionError { error, .. } => return Err(PairError::Unreachable(error.to_string())),
                _ => {}
            }
        }
    })
    .await
    .map_err(|_| PairError::Unreachable("timed out (is the other Mac awake and showing a code?)".into()))??;
    let control = swarm.behaviour().stream.new_control();
    let task = tokio::spawn(async move {
        loop {
            swarm.select_next_some().await;
        }
    });
    Ok(PairConnection {
        remote,
        local,
        control,
        task,
    })
}

/// Client side of pairing: dial, run SPAKE2 + confirmation, store the other Mac.
pub async fn pair_with(
    node: &NodeKeypair,
    store: &TrustStore,
    addr: &Multiaddr,
    code: &PairingCode,
    info: PairInfo,
    paired: &PairedConfig,
) -> Result<TrustedPeer, PairError> {
    pair_with_checked(node, store, std::slice::from_ref(addr), code, info, paired, None).await
}

/// Dial every address at once; the first QUIC connection wins (LAN discovery can list
/// several addresses, some unreachable from here).
pub async fn connect_first(
    node: &NodeKeypair,
    addrs: &[Multiaddr],
    paired: &PairedConfig,
) -> Result<(PairConnection, Multiaddr), PairError> {
    if addrs.is_empty() {
        return Err(PairError::Unreachable("no address to try".into()));
    }
    let attempts = addrs.iter().map(|a| {
        Box::pin(async move { connect(node, a, paired).await.map(|c| (c, a.clone())) })
    });
    match futures::future::select_ok(attempts).await {
        Ok((won, _rest)) => Ok(won),
        Err(e) => Err(e),
    }
}

/// [`pair_with`], and when `expect_fp` is set (the Mac was picked from LAN discovery),
/// the connected PeerId's fingerprint must start with it — checked right after the QUIC
/// handshake, before any pairing message (so a different Mac answering at a discovered
/// address costs the real window nothing).
pub async fn pair_with_checked(
    node: &NodeKeypair,
    store: &TrustStore,
    addrs: &[Multiaddr],
    code: &PairingCode,
    info: PairInfo,
    paired: &PairedConfig,
    expect_fp: Option<&str>,
) -> Result<TrustedPeer, PairError> {
    let (conn, addr) = connect_first(node, addrs, paired).await?;
    let addr = &addr;
    if let Some(fp) = expect_fp {
        let got = crate::lan::fingerprint(&conn.remote);
        if !got.starts_with(fp) {
            return Err(PairError::Protocol(format!(
                "the Mac at {addr} has fingerprint {got}, not {fp}; refusing to pair"
            )));
        }
    }
    let mut control = conn.control.clone();
    let stream = tokio::time::timeout(Duration::from_secs(3), control.open_stream(conn.remote, PAIR_PROTOCOL_V1))
        .await
        .map_err(|_| PairError::Unreachable("pairing stream timed out".into()))?
        .map_err(|e| PairError::Refused {
            code: "not_pairing".into(),
            message: format!("The other Mac is not accepting pairing right now ({e})."),
        })?;
    let mut io = stream.compat();
    let (id_a, id_b) = (conn.local.to_bytes(), conn.remote.to_bytes());
    let (start, spake_a) = PakeStart::new(PakeRole::Client, code, &id_a, &id_b);
    let x25519_a = node.public_bytes();
    write_msg(
        &mut io,
        &PairMsg::Start {
            spake: hex::encode(&spake_a),
            x25519: hex::encode(x25519_a),
        },
    )
    .await?;
    let (spake_b, x25519_b, sealed_b, confirm_b) = match read_msg(&mut io).await? {
        PairMsg::Reply { spake, x25519, info, confirm } => {
            (unhex_vec(&spake)?, unhex::<32>(&x25519)?, unhex_vec(&info)?, unhex_vec(&confirm)?)
        }
        PairMsg::Refused { code, message } => {
            return Err(PairError::Refused {
                code,
                message: p2p_forward::allowlist::display_safe(&message),
            })
        }
        _ => return Err(PairError::Protocol("unexpected reply".into())),
    };
    let key = start.finish(&spake_b)?;
    let t = Transcript {
        peer_id_a: &id_a,
        peer_id_b: &id_b,
        x25519_a,
        x25519_b,
    };
    // Verify B first: a wrong code or a relay stops here, before we reveal anything.
    let info_b = key.open_info(&sealed_b)?;
    key.verify_peer(&t, &info_b, &[], &confirm_b)?;
    let confirm_a = key.confirm(&t, &info, &confirm_b);
    write_msg(
        &mut io,
        &PairMsg::Finish {
            info: hex::encode(key.seal_info(&info)),
            confirm: hex::encode(confirm_a),
        },
    )
    .await?;
    match read_msg(&mut io).await? {
        PairMsg::Done { ack } => key.open_ack(&unhex_vec(&ack)?)?,
        PairMsg::Refused { code, message } => {
            return Err(PairError::Refused {
                code,
                message: p2p_forward::allowlist::display_safe(&message),
            })
        }
        _ => return Err(PairError::Protocol("unexpected final message".into())),
    }
    let pinned = strip_p2p(addr).with(Protocol::P2p(conn.remote));
    let entry = TrustedPeer {
        peer_id: conn.remote.to_string(),
        identity_hex: identity_hex(&conn.remote),
        encryption_node_id: hex::encode(x25519_b),
        name: clean_name(&info_b.name),
        role: PeerRole::Share,
        paired_at: unix_now(),
        last_addrs: vec![pinned.to_string()],
        last_seen: Some(unix_now()),
        app_version: clean_name(&info_b.app_version),
        may_start_model: info_b.may_start_model,
        wake_macs: info_b.macs.clone(),
    };
    store.add(entry.clone())?;
    Ok(entry)
}

/// `addr` without a trailing `/p2p/<id>`.
pub fn strip_p2p(addr: &Multiaddr) -> Multiaddr {
    addr.iter().filter(|p| !matches!(p, Protocol::P2p(_))).collect()
}

/// Our claims for a pairing.
pub fn local_info(name: Option<String>, may_start_model: bool) -> PairInfo {
    PairInfo {
        name: clean_name(&name.unwrap_or_else(p2p_forward::status::default_name)),
        app_version: env!("CARGO_PKG_VERSION").into(),
        may_start_model,
        macs: Vec::new(),
    }
}

/// The sharing Mac's claims: [`local_info`] + its Ethernet / Wi-Fi MAC addresses, so the
/// other Mac can wake it (M5; sent encrypted and bound into the confirmation tag).
pub fn share_info(name: Option<String>, may_start_model: bool) -> PairInfo {
    PairInfo {
        macs: crate::wake::local_macs(),
        ..local_info(name, may_start_model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_counts_armed_failures_and_closes() {
        let clock = Arc::new(ManualClock::default());
        let w = PairingWindow::open(clock.clone(), Default::default());
        // unarmed sessions (nothing sent) do not count
        drop(w.begin().unwrap());
        assert_eq!(w.failed_attempts(), 0);
        let a = w.begin().unwrap();
        assert_eq!(w.begin().err().unwrap().code, "busy", "one session at a time");
        drop(a);
        for i in 1..=3 {
            let mut a = w.begin().unwrap();
            a.arm();
            drop(a);
            assert_eq!(w.failed_attempts(), i);
        }
        assert_eq!(w.state(), WindowState::TooManyFailures);
        assert_eq!(w.begin().err().unwrap().code, "too_many_attempts");
    }

    #[test]
    fn window_expires_after_five_minutes_and_closes_after_success() {
        let clock = Arc::new(ManualClock::default());
        let w = PairingWindow::open(clock.clone(), Default::default());
        clock.advance(PAIRING_WINDOW - Duration::from_secs(1));
        assert!(w.is_open());
        clock.advance(Duration::from_secs(1));
        assert_eq!(w.state(), WindowState::Expired);
        assert_eq!(w.begin().err().unwrap().code, "expired");
        let w = PairingWindow::open(clock.clone(), Default::default());
        let mut a = w.begin().unwrap();
        a.arm();
        let node = NodeKeypair::generate();
        a.succeed(crate::trust::TrustedPeer {
            peer_id: p2p_forward::transport_identity(&node).public().to_peer_id().to_string(),
            identity_hex: String::new(),
            encryption_node_id: node.public_hex(),
            name: "x".into(),
            role: PeerRole::Use,
            paired_at: 0,
            last_addrs: vec![],
            last_seen: None,
            app_version: "0".into(),
            may_start_model: true,
            wake_macs: vec![],
        });
        assert_eq!(w.state(), WindowState::Paired);
        assert_eq!(w.failed_attempts(), 0);
        assert_eq!(w.begin().err().unwrap().code, "already_used");
    }

    #[test]
    fn identity_hex_and_names() {
        let node = NodeKeypair::generate();
        let id = p2p_forward::transport_identity(&node);
        let peer = id.public().to_peer_id();
        let expect = hex::encode(id.public().try_into_ed25519().unwrap().to_bytes());
        assert_eq!(identity_hex(&peer), expect);
        assert_eq!(clean_name("Mac\u{1b}[31m Studio\n\u{202E}"), "Mac[31m Studio");
        assert_eq!(clean_name(""), "Mac");
        assert_eq!(clean_name(&"x".repeat(200)).len(), 64);
    }
}
