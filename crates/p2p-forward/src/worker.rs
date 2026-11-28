//! Share side ("big Mac", plan §3 `worker.rs`): accepts `/slipstream/openai/1` and
//! `/slipstream/status/1` streams from allowed peers only, re-checks the allowlist,
//! applies per-peer admission, forwards to the local llama-server and pipes the raw
//! response bytes back. A `Cancel` frame, a stream reset or peer loss drops the upstream
//! connection at once, which is how llama-server learns that the client went away
//! (it then cancels the task, including a layer-major prefill).
//!
//! Hardening (M1 security review): the upstream request line is rebuilt from the
//! allowlist's constant method/path ([`allowlist::canonical`]) — the peer's target bytes
//! never reach llama-server; request bodies are capped at [`MAX_BODY`] (declared and
//! streamed/chunked); light routes have their own admission; the SealedStream handshake
//! and the first frame must arrive within 5 s, except for at most
//! [`ShareLimits::idle_streams_per_peer`] idle (pre-`ReqHead`) streams per peer (the use
//! side's pre-opened spare); `GET /props` and `GET /slots` are sanitized.
//!
//! Trust (M2): the swarm runs the trust gate ([`crate::gate`]); every accepted stream
//! re-checks [`PeerTrust::trusted_key`] and uses the pinned static key for the
//! authenticated SealedStream handshake; a request whose peer loses trust mid-stream is
//! cancelled at once ([`crate::trust::until_untrusted`]).

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId};
use p2p_crypto::{NodeKeypair, StreamRole};
use p2p_libp2p::paired::{Control, PairedConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::admission::{Admission, AdmissionLimits, Busy};
use crate::allowlist::{self, Canonical, Route};
use crate::http;
use crate::proto::{
    channel_context, open_channel, BodyKind, ChannelReader, ChannelWriter, FrameType, ReqHead,
    BODY_CHUNK, MAX_BODY, OPENAI_PROTOCOL_V1, STATUS_PROTOCOL_V1,
};
use crate::status::{self, StatusRequest};
use crate::trust::{until_untrusted, PeerTrust};
use crate::unix_ms;

pub const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Hard cap per forwarded request (plan §3 timeouts: no first-byte timeout, total 2 h).
pub const REQUEST_TOTAL_CAP: Duration = Duration::from_secs(2 * 60 * 60);
/// Largest `/slots` / `/props` response buffered for sanitizing.
const SLOTS_MAX: usize = 4 * 1024 * 1024;

/// Share-side stream limits (M1 security review).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShareLimits {
    /// Request body cap (declared `Content-Length` and streamed/chunked bytes).
    pub max_body: u64,
    /// The SealedStream handshake must finish within this time.
    pub handshake_timeout: Duration,
    /// The first frame (`ReqHead` / `StatusReq`) must arrive within this time, unless
    /// the stream gets one of the per-peer idle places.
    pub first_frame_timeout: Duration,
    /// Streams per peer allowed to stay idle (handshaken, no `ReqHead` yet) beyond
    /// `first_frame_timeout` — the use side keeps one pre-opened spare.
    pub idle_streams_per_peer: usize,
}

impl Default for ShareLimits {
    fn default() -> Self {
        Self {
            max_body: MAX_BODY,
            handshake_timeout: Duration::from_secs(5),
            first_frame_timeout: Duration::from_secs(5),
            idle_streams_per_peer: 2,
        }
    }
}

#[derive(Clone)]
pub struct ShareConfig {
    pub listen: Multiaddr,
    /// The local llama-server (loopback only).
    pub upstream: SocketAddr,
    /// M2 trust gate (the paired Macs from the trust store).
    pub trust: Arc<dyn PeerTrust>,
    /// This node's static X25519 key (`node.key`), for the authenticated SealedStream.
    pub node: NodeKeypair,
    pub paired: PairedConfig,
    pub admission: AdmissionLimits,
    pub limits: ShareLimits,
    pub name: String,
    /// App-side model lifecycle for `StartModel` / `StopModel` (M4); `None` = headless.
    pub model: Option<Arc<dyn crate::status::ModelControl>>,
    pub verbose: bool,
}

/// Parse `--forward`: `http://127.0.0.1:8080` or `127.0.0.1:8080`. Loopback only, so
/// a shared Mac can never be used as a relay into its own network.
pub fn parse_upstream(s: &str) -> Result<SocketAddr, String> {
    let rest = s.strip_prefix("http://").unwrap_or(s);
    let rest = rest.trim_end_matches('/');
    let rest = rest.replace("localhost", "127.0.0.1");
    let addr: SocketAddr = rest
        .parse()
        .map_err(|_| format!("--forward must look like http://127.0.0.1:8080, got {s}"))?;
    if !addr.ip().is_loopback() {
        return Err(format!(
            "--forward must point at this Mac's own llama-server (loopback), got {s}"
        ));
    }
    Ok(addr)
}

#[derive(Debug, Default, Clone)]
pub struct ShareStats {
    pub connections_established: u64,
    pub connections_rejected: u64,
    pub connections_closed: u64,
    pub streams_rejected: u64,
    pub requests: u64,
    pub forbidden: u64,
    pub busy: u64,
    pub completed: u64,
    pub cancelled: u64,
    pub upstream_errors: u64,
    pub status_requests: u64,
    pub body_too_large: u64,
    pub idle_streams_refused: u64,
    pub handshake_timeouts: u64,
    /// Streams that passed the per-stream trust check (handshake started).
    pub streams_accepted: u64,
    /// Untrusted peers admitted while a pairing window was open.
    pub pairing_guests: u64,
    /// Guests disconnected after the pairing window closed.
    pub guests_closed: u64,
    /// Connections closed because the peer lost trust (revoke / unpair).
    pub revoked_disconnects: u64,
    /// Requests aborted because the peer lost trust mid-stream.
    pub revoked_streams: u64,
}

pub type SharedShareStats = Arc<Mutex<ShareStats>>;

pub struct ShareHandle {
    pub peer_id: PeerId,
    pub listen_addr: Multiaddr,
    pub stats: SharedShareStats,
    pub admission: Admission,
    control: Control,
    tasks: Vec<JoinHandle<()>>,
}

impl ShareHandle {
    /// Full pinned address for the use side: `<listen>/p2p/<peer_id>`.
    pub fn dial_addr(&self) -> Multiaddr {
        self.listen_addr.clone().with(Protocol::P2p(self.peer_id))
    }

    /// The swarm's stream control (to register more protocols, e.g. `/slipstream/pair/1`).
    pub fn control(&self) -> Control {
        self.control.clone()
    }
}

impl Drop for ShareHandle {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

fn other<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(e.to_string())
}

struct Ctx {
    local: PeerId,
    node: NodeKeypair,
    trust: Arc<dyn PeerTrust>,
    upstream: SocketAddr,
    name: String,
    model: Option<Arc<dyn crate::status::ModelControl>>,
    admission: Admission,
    limits: ShareLimits,
    idle: Arc<Mutex<HashMap<PeerId, usize>>>,
    stats: SharedShareStats,
    verbose: bool,
}

/// One of a peer's idle-stream places; released on drop.
struct IdleSlot {
    idle: Arc<Mutex<HashMap<PeerId, usize>>>,
    peer: PeerId,
}

impl Drop for IdleSlot {
    fn drop(&mut self) {
        if let Some(n) = self.idle.lock().unwrap().get_mut(&self.peer) {
            *n = n.saturating_sub(1);
        }
    }
}

impl Ctx {
    fn idle_slot(&self, peer: PeerId) -> Option<IdleSlot> {
        let mut idle = self.idle.lock().unwrap();
        let n = idle.entry(peer).or_insert(0);
        if *n >= self.limits.idle_streams_per_peer {
            return None;
        }
        *n += 1;
        Some(IdleSlot {
            idle: self.idle.clone(),
            peer,
        })
    }

    /// Handshake (bounded) + first frame: within `first_frame_timeout`, or later if the
    /// stream holds an idle place (released as soon as the frame starts arriving). A
    /// started frame must complete within `first_frame_timeout` (slow-loris guard).
    async fn open_and_first_frame(
        &self,
        peer: PeerId,
        peer_key: [u8; 32],
        stream: libp2p::Stream,
        protocol: &libp2p::StreamProtocol,
    ) -> io::Result<Option<(crate::proto::Frame, ChannelReader, ChannelWriter)>> {
        let context = channel_context(&peer, &self.local, protocol);
        let (mut reader, writer) = match tokio::time::timeout(
            self.limits.handshake_timeout,
            open_channel(stream, StreamRole::Responder, &context, &self.node, &peer_key),
        )
        .await
        {
            Ok(r) => r?,
            Err(_) => {
                self.stats.lock().unwrap().handshake_timeouts += 1;
                return Err(io::Error::new(io::ErrorKind::TimedOut, "stream handshake timed out"));
            }
        };
        let readable =
            match tokio::time::timeout(self.limits.first_frame_timeout, reader.wait_readable()).await {
                Ok(r) => r?,
                Err(_) => {
                    let Some(_slot) = self.idle_slot(peer) else {
                        self.stats.lock().unwrap().idle_streams_refused += 1;
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "idle stream refused (per-peer idle cap)",
                        ));
                    };
                    reader.wait_readable().await?
                }
            };
        if !readable {
            return Ok(None);
        }
        let frame = tokio::time::timeout(self.limits.first_frame_timeout, reader.next())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "first frame timed out"))??;
        Ok(frame.map(|f| (f, reader, writer)))
    }
}

/// Log at most one line per `interval`; the lines in between are counted.
#[derive(Debug)]
pub struct LogRateLimit {
    interval: Duration,
    last: Option<Instant>,
    suppressed: u64,
}

impl LogRateLimit {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: None,
            suppressed: 0,
        }
    }

    /// `Some(suppressed since the last logged line)` if this event may be logged.
    pub fn allow(&mut self, now: Instant) -> Option<u64> {
        match self.last {
            Some(t) if now.duration_since(t) < self.interval => {
                self.suppressed += 1;
                None
            }
            _ => {
                self.last = Some(now);
                Some(std::mem::take(&mut self.suppressed))
            }
        }
    }
}

pub async fn start_share(
    identity: libp2p::identity::Keypair,
    cfg: ShareConfig,
) -> io::Result<ShareHandle> {
    if !cfg.upstream.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "upstream must be a loopback address",
        ));
    }
    let stats = SharedShareStats::default();
    let gated = crate::gate::start_gated_listener(
        identity,
        cfg.listen.clone(),
        &cfg.paired,
        cfg.trust.clone(),
        stats.clone(),
        "share",
        cfg.verbose,
    )
    .await?;
    let local = gated.peer_id;
    let listen_addr = gated.listen_addr.clone();
    let mut control = gated.control.clone();
    let mut openai = control.accept(OPENAI_PROTOCOL_V1).map_err(other)?;
    let mut status_in = control.accept(STATUS_PROTOCOL_V1).map_err(other)?;
    let admission = Admission::new(cfg.admission);
    let ctx = Arc::new(Ctx {
        local,
        node: cfg.node.clone(),
        trust: cfg.trust.clone(),
        upstream: cfg.upstream,
        name: cfg.name.clone(),
        model: cfg.model.clone(),
        admission: admission.clone(),
        limits: cfg.limits,
        idle: Arc::default(),
        stats: stats.clone(),
        verbose: cfg.verbose,
    });

    let openai_ctx = ctx.clone();
    let openai_task = tokio::spawn(async move {
        // Spawn per stream immediately: libp2p-stream drops inbound streams silently if
        // the acceptor is not polling (M0 finding).
        while let Some((peer, stream)) = openai.next().await {
            // Per-stream trust check (a pairing guest or a just-revoked peer gets nothing).
            let Some(key) = openai_ctx.trust.trusted_key(&peer) else {
                openai_ctx.stats.lock().unwrap().streams_rejected += 1;
                drop(stream);
                continue;
            };
            openai_ctx.stats.lock().unwrap().streams_accepted += 1;
            let ctx = openai_ctx.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_openai(&ctx, peer, key, stream).await {
                    if ctx.verbose {
                        eprintln!("share event=stream_error peer={peer} error={e}");
                    }
                }
            });
        }
    });

    let status_ctx = ctx.clone();
    let status_task = tokio::spawn(async move {
        while let Some((peer, stream)) = status_in.next().await {
            let Some(key) = status_ctx.trust.trusted_key(&peer) else {
                status_ctx.stats.lock().unwrap().streams_rejected += 1;
                continue;
            };
            status_ctx.stats.lock().unwrap().streams_accepted += 1;
            let ctx = status_ctx.clone();
            tokio::spawn(async move {
                let _ = handle_status(&ctx, peer, key, stream).await;
            });
        }
    });

    eprintln!(
        "share event=listening addr={} upstream={}",
        listen_addr.clone().with(Protocol::P2p(local)),
        cfg.upstream,
    );
    Ok(ShareHandle {
        peer_id: local,
        listen_addr,
        stats,
        admission,
        control: gated.control.clone(),
        tasks: vec![gated.task, openai_task, status_task],
    })
}

async fn handle_status(ctx: &Ctx, peer: PeerId, peer_key: [u8; 32], stream: libp2p::Stream) -> io::Result<()> {
    let context = channel_context(&peer, &ctx.local, &STATUS_PROTOCOL_V1);
    let (mut reader, mut writer) = tokio::time::timeout(
        ctx.limits.handshake_timeout,
        open_channel(stream, StreamRole::Responder, &context, &ctx.node, &peer_key),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "stream handshake timed out"))??;
    let frame = tokio::time::timeout(ctx.limits.first_frame_timeout, reader.next())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "status request timed out"))??;
    let Some(frame) = frame.filter(|f| f.kind == FrameType::StatusReq) else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "expected StatusReq"));
    };
    let req: StatusRequest = serde_json::from_slice(&frame.payload)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    ctx.stats.lock().unwrap().status_requests += 1;
    let reply = status::answer(req, &ctx.name, ctx.upstream, ctx.admission.load(), peer, ctx.model.as_ref()).await;
    writer.send_json(FrameType::StatusResp, &reply).await?;
    writer.finish().await
}

/// Send a complete synthetic HTTP response as RespHead + RespBody + end + FIN.
async fn send_response(writer: &mut ChannelWriter, bytes: &[u8]) -> io::Result<()> {
    let end = http::find_head_end(bytes).unwrap_or(bytes.len());
    writer.send(FrameType::RespHead, &bytes[..end]).await?;
    writer.send_body(FrameType::RespBody, &bytes[end..]).await?;
    writer.send(FrameType::RespBody, &[]).await?;
    writer.finish().await
}

enum Outcome {
    Completed,
    Cancelled,
    UpstreamError,
}

async fn handle_openai(ctx: &Ctx, peer: PeerId, peer_key: [u8; 32], stream: libp2p::Stream) -> io::Result<()> {
    // Handshake ≤ 5 s; the first frame ≤ 5 s unless this is one of the peer's (≤ 2) idle
    // streams — the use side keeps one pre-opened spare that may sit idle for hours
    // (connection liveness comes from QUIC keep-alive).
    let Some((first, reader, mut writer)) =
        ctx.open_and_first_frame(peer, peer_key, stream, &OPENAI_PROTOCOL_V1).await?
    else {
        return Ok(());
    };
    if first.kind != FrameType::ReqHead {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "expected ReqHead"));
    }
    let head: ReqHead = serde_json::from_slice(&first.payload)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    ctx.stats.lock().unwrap().requests += 1;
    // Allowlist, second time (the use side already checked), and rebuild the request
    // from constants: the peer's method/target bytes are never written upstream.
    let Some(canon) = allowlist::canonical(&head.method, &head.target) else {
        ctx.stats.lock().unwrap().forbidden += 1;
        eprintln!(
            "share event=forbidden peer={peer} method={} path={}",
            sanitize_log(&head.method),
            sanitize_log(allowlist::path_of(&head.target))
        );
        return send_response(&mut writer, &http::forbidden()).await;
    };
    let route = canon.route;
    let path = canon.path;
    if let BodyKind::Length(n) = head.body {
        if n > ctx.limits.max_body {
            ctx.stats.lock().unwrap().body_too_large += 1;
            return send_response(&mut writer, &body_too_large()).await;
        }
    }

    // Request-direction reader: body chunks → channel, Cancel / reset / FIN → cancel.
    let cancel = CancellationToken::new();
    let cancel_at: Arc<Mutex<Option<Instant>>> = Arc::default();
    let (body_tx, body_rx) = mpsc::channel::<Vec<u8>>(8);
    let too_large = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Revoke takes effect mid-request: the peer losing trust cancels at once (the
    // upstream connection is dropped; the gate closes the QUIC connection right after).
    let revoked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let revoke_task = tokio::spawn({
        let trust = ctx.trust.clone();
        let cancel = cancel.clone();
        let revoked = revoked.clone();
        async move {
            until_untrusted(&*trust, peer, peer_key).await;
            revoked.store(true, std::sync::atomic::Ordering::SeqCst);
            cancel.cancel();
        }
    });
    let _revoke_guard = AbortOnDrop(revoke_task);
    let reader_task = tokio::spawn(read_request_side(
        reader,
        body_tx,
        cancel.clone(),
        cancel_at.clone(),
        ctx.limits.max_body,
        too_large.clone(),
    ));

    let t0 = Instant::now();
    let _light_permit = if route == Route::Inference {
        None
    } else {
        tokio::select! {
            p = ctx.admission.acquire_light(peer) => match p {
                Ok(p) => Some(p),
                Err(Busy) => {
                    ctx.stats.lock().unwrap().busy += 1;
                    eprintln!("share event=busy_light peer={peer} path={path}");
                    reader_task.abort();
                    return send_response(&mut writer, &http::error_response(
                        503,
                        "busy",
                        "The shared Mac is busy with other requests from this Mac. Try again in a few seconds.",
                    )).await;
                }
            },
            _ = cancel.cancelled() => {
                ctx.stats.lock().unwrap().cancelled += 1;
                reader_task.abort();
                return Ok(());
            }
        }
    };
    let _permit = if route == Route::Inference {
        tokio::select! {
            p = ctx.admission.acquire(peer) => match p {
                Ok(p) => Some(p),
                Err(Busy) => {
                    ctx.stats.lock().unwrap().busy += 1;
                    eprintln!("share event=busy peer={peer} path={path}");
                    reader_task.abort();
                    return send_response(&mut writer, &http::error_response(
                        503,
                        "busy",
                        "The shared Mac is busy with other requests from this Mac. Try again in a few seconds.",
                    )).await;
                }
            },
            _ = cancel.cancelled() => {
                ctx.stats.lock().unwrap().cancelled += 1;
                reader_task.abort();
                return Ok(());
            }
        }
    } else {
        None
    };

    let outcome = tokio::select! {
        r = tokio::time::timeout(REQUEST_TOTAL_CAP, forward(ctx, &head, canon, body_rx, &mut writer)) => match r {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                if ctx.verbose { eprintln!("share event=forward_error peer={peer} error={e}"); }
                Outcome::Cancelled
            }
            Err(_) => Outcome::Cancelled,
        },
        // Dropping `forward` drops the upstream TcpStream → llama-server sees the client
        // disconnect and cancels the task.
        _ = cancel.cancelled() => Outcome::Cancelled,
    };
    reader_task.abort();
    if too_large.load(std::sync::atomic::Ordering::SeqCst) {
        // Upstream connection already dropped (cancel). Answer 413 if the editor has
        // not seen a response head yet.
        ctx.stats.lock().unwrap().body_too_large += 1;
        eprintln!("share event=body_too_large peer={peer} path={path}");
        if writer.frames_sent() == 0 {
            return send_response(&mut writer, &body_too_large()).await;
        }
        return Ok(());
    }
    let mut s = ctx.stats.lock().unwrap();
    if revoked.load(std::sync::atomic::Ordering::SeqCst) {
        s.revoked_streams += 1;
        eprintln!("share event=revoked_stream_aborted peer={peer} path={path} unix_ms={}", unix_ms());
    }
    match outcome {
        Outcome::Completed => s.completed += 1,
        Outcome::Cancelled => {
            s.cancelled += 1;
            let lag = cancel_at
                .lock()
                .unwrap()
                .map(|t| t.elapsed().as_secs_f64() * 1e3);
            eprintln!(
                "share event=cancelled peer={peer} path={path} after_ms={:.1} upstream_dropped_after_cancel_ms={} unix_ms={}",
                t0.elapsed().as_secs_f64() * 1e3,
                lag.map(|l| format!("{l:.2}")).unwrap_or_else(|| "na".into()),
                unix_ms()
            );
        }
        Outcome::UpstreamError => s.upstream_errors += 1,
    }
    Ok(())
}

struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn body_too_large() -> Vec<u8> {
    http::error_response(413, "body_too_large", "Request body larger than 32 MiB.")
}

/// Keep logs free of prompt content: paths/methods only, bounded, printable.
fn sanitize_log(path: &str) -> String {
    path.chars()
        .take(64)
        .map(|c| if c.is_ascii_graphic() { c } else { '?' })
        .collect()
}

async fn read_request_side(
    mut reader: ChannelReader,
    body_tx: mpsc::Sender<Vec<u8>>,
    cancel: CancellationToken,
    cancel_at: Arc<Mutex<Option<Instant>>>,
    max_body: u64,
    too_large: Arc<std::sync::atomic::AtomicBool>,
) {
    let mut body_tx = Some(body_tx);
    let mut received: u64 = 0;
    loop {
        match reader.next().await {
            Ok(Some(frame)) => match frame.kind {
                FrameType::ReqBody if frame.payload.is_empty() => {
                    body_tx = None;
                }
                FrameType::ReqBody => {
                    received += frame.payload.len() as u64;
                    if received > max_body {
                        too_large.store(true, std::sync::atomic::Ordering::SeqCst);
                        break;
                    }
                    if let Some(tx) = &body_tx {
                        if tx.send(frame.payload).await.is_err() {
                            body_tx = None;
                        }
                    }
                }
                _ => break, // Cancel or anything unexpected
            },
            // FIN before the response ended, reset, or peer loss.
            Ok(None) | Err(_) => break,
        }
    }
    *cancel_at.lock().unwrap() = Some(Instant::now());
    cancel.cancel();
}

/// The upstream request head. Method and target come from the allowlist constants
/// ([`Canonical`]); header names/values were filtered (token names, no control bytes).
fn upstream_request(canon: Canonical, head: &ReqHead, upstream: SocketAddr) -> Vec<u8> {
    let mut out = format!(
        "{} {} HTTP/1.1\r\nHost: {upstream}\r\n",
        canon.method,
        canon.target()
    );
    for (k, v) in allowlist::filter_request_headers(&head.headers) {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    match head.body {
        BodyKind::None => {
            if canon.method == "POST" {
                out.push_str("Content-Length: 0\r\n");
            }
        }
        BodyKind::Length(n) => out.push_str(&format!("Content-Length: {n}\r\n")),
        BodyKind::Chunked => out.push_str("Transfer-Encoding: chunked\r\n"),
    }
    // One request per upstream connection: the response ends when llama-server closes,
    // and closing it is how a cancel reaches the engine.
    out.push_str("Connection: close\r\n\r\n");
    out.into_bytes()
}

async fn pump_body(
    mut up_w: tokio::net::tcp::OwnedWriteHalf,
    mut body_rx: mpsc::Receiver<Vec<u8>>,
    kind: BodyKind,
) -> io::Result<()> {
    let mut sent: u64 = 0;
    while let Some(chunk) = body_rx.recv().await {
        sent += chunk.len() as u64;
        match kind {
            BodyKind::Length(n) if sent > n => {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "body longer than declared"));
            }
            BodyKind::None => {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "unexpected body"));
            }
            BodyKind::Chunked => {
                up_w.write_all(format!("{:x}\r\n", chunk.len()).as_bytes()).await?;
                up_w.write_all(&chunk).await?;
                up_w.write_all(b"\r\n").await?;
            }
            BodyKind::Length(_) => up_w.write_all(&chunk).await?,
        }
    }
    if kind == BodyKind::Chunked {
        up_w.write_all(b"0\r\n\r\n").await?;
    }
    up_w.flush().await?;
    // Keep the write half open (no shutdown): a half-close could be read as a client
    // disconnect by the server. It is dropped together with the read half.
    std::future::pending::<()>().await;
    Ok(())
}

async fn forward(
    ctx: &Ctx,
    head: &ReqHead,
    canon: Canonical,
    body_rx: mpsc::Receiver<Vec<u8>>,
    writer: &mut ChannelWriter,
) -> io::Result<Outcome> {
    let upstream = match tokio::time::timeout(
        UPSTREAM_CONNECT_TIMEOUT,
        TcpStream::connect(ctx.upstream),
    )
    .await
    {
        Ok(Ok(s)) => s,
        _ => {
            send_response(
                writer,
                &http::error_response(
                    503,
                    "model_not_running",
                    "The model is not running on the shared Mac.",
                ),
            )
            .await?;
            return Ok(Outcome::UpstreamError);
        }
    };
    upstream.set_nodelay(true)?;
    let (mut up_r, mut up_w) = upstream.into_split();
    up_w.write_all(&upstream_request(canon, head, ctx.upstream)).await?;
    let body = pump_body(up_w, body_rx, head.body);
    tokio::pin!(body);
    let resp = async {
        match canon.route {
            Route::SlotsSanitized => return pump_sanitized(&mut up_r, writer, allowlist::sanitize_slots).await,
            Route::PropsSanitized => return pump_sanitized(&mut up_r, writer, allowlist::sanitize_props).await,
            Route::Inference | Route::Light => {}
        }
        pump_response(&mut up_r, writer).await
    };
    tokio::pin!(resp);
    loop {
        tokio::select! {
            r = &mut body => {
                if let Err(e) = r {
                    if ctx.verbose { eprintln!("share event=body_error error={e}"); }
                    return Ok(Outcome::Cancelled);
                }
            }
            r = &mut resp => return r,
        }
    }
}

/// Raw byte pipe: upstream head → RespHead, every upstream read → one RespBody frame.
async fn pump_response(
    up_r: &mut tokio::net::tcp::OwnedReadHalf,
    writer: &mut ChannelWriter,
) -> io::Result<Outcome> {
    let Some((head, rest)) = http::read_head(up_r).await? else {
        send_response(
            writer,
            &http::error_response(502, "upstream_closed", "The shared Mac's model server closed the connection."),
        )
        .await?;
        return Ok(Outcome::UpstreamError);
    };
    writer.send(FrameType::RespHead, &head).await?;
    writer.send_body(FrameType::RespBody, &rest).await?;
    let mut buf = vec![0u8; BODY_CHUNK];
    loop {
        let n = up_r.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        writer.send(FrameType::RespBody, &buf[..n]).await?;
    }
    writer.send(FrameType::RespBody, &[]).await?;
    writer.finish().await?;
    Ok(Outcome::Completed)
}

/// `GET /slots` / `GET /props`: buffer, reduce to non-sensitive fields, answer with
/// Content-Length.
async fn pump_sanitized(
    up_r: &mut tokio::net::tcp::OwnedReadHalf,
    writer: &mut ChannelWriter,
    sanitize: fn(&[u8]) -> Vec<u8>,
) -> io::Result<Outcome> {
    let mut raw = Vec::new();
    up_r.take(SLOTS_MAX as u64 + 1).read_to_end(&mut raw).await?;
    let parsed = http::find_head_end(&raw).and_then(|end| {
        let head = http::parse_response_head(&raw[..end]).ok()?;
        let body = if head.is_chunked() {
            http::dechunk_strict(&raw[end..], SLOTS_MAX)?
        } else {
            raw[end..].to_vec()
        };
        Some((head.status, body))
    });
    let Some((status, body)) = parsed.filter(|_| raw.len() <= SLOTS_MAX) else {
        send_response(
            writer,
            &http::error_response(502, "bad_upstream", "Unexpected answer from the shared Mac's model server."),
        )
        .await?;
        return Ok(Outcome::UpstreamError);
    };
    send_response(writer, &http::json_response(status, &[], &sanitize(&body)))
        .await?;
    Ok(Outcome::Completed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// M1 review finding 1: the upstream request line is built from constants only.
    #[test]
    fn upstream_request_line_is_constant() {
        let head = ReqHead {
            method: "POST".into(),
            target: "/v1/chat/completions?x=%0d%0aPOST%20/slots/0?action=save".into(),
            headers: vec![
                ("content-type".into(), "application/json".into()),
                ("x-bad".into(), "a\r\nPOST /slots/0?action=save HTTP/1.1".into()),
            ],
            body: BodyKind::Length(2),
        };
        let canon = allowlist::canonical(&head.method, &head.target).unwrap();
        let req = String::from_utf8(upstream_request(canon, &head, "127.0.0.1:8080".parse().unwrap())).unwrap();
        assert_eq!(
            req,
            "POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:8080\r\ncontent-type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n"
        );
        assert_eq!(req.matches("HTTP/1.1").count(), 1);
    }

    #[test]
    fn log_rate_limit_counts_suppressed_lines() {
        let mut rl = LogRateLimit::new(Duration::from_secs(10));
        let t0 = Instant::now();
        assert_eq!(rl.allow(t0), Some(0));
        for i in 1..100 {
            assert_eq!(rl.allow(t0 + Duration::from_millis(i * 10)), None);
        }
        assert_eq!(rl.allow(t0 + Duration::from_secs(11)), Some(99));
        assert_eq!(rl.allow(t0 + Duration::from_secs(12)), None);
    }

    #[test]
    fn sanitize_log_bounds_method_and_path() {
        assert_eq!(sanitize_log("GET\r\nX: SECRET"), "GET??X:?SECRET");
        assert_eq!(sanitize_log(&"A".repeat(500)).len(), 64);
    }
}
