//! Use side ("MacBook", plan §3 `client.rs`): serves the editor on `127.0.0.1:<port>`
//! and forwards each allowed request over one stream of a persistent, pinned QUIC
//! connection to the share side.
//!
//! Local guards (before anything leaves this Mac): loopback bind only, `Host`
//! allowlist and browser-origin refusal (DNS rebinding / web pages), method/path
//! allowlist, credential + hop-by-hop header stripping, 32 MiB streamed body cap.
//! Latency: TCP_NODELAY, one flush per response frame, one pre-opened + handshaken
//! spare stream per connection (hides stream negotiation after idle, M0 finding).
//! Cancellation: when the editor closes its connection (stop button), a `Cancel`
//! frame is sent and the stream dropped; the share side drops its upstream connection.

use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use libp2p::multiaddr::Protocol;
use libp2p::swarm::SwarmEvent;
use libp2p::{Multiaddr, PeerId};
use p2p_crypto::StreamRole;
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
    WireError, BODY_CHUNK, OPENAI_PROTOCOL_V1, STATUS_PROTOCOL_V1,
};
use crate::status::{StatusReply, StatusRequest};
use crate::unix_ms;

pub const DIAL_TIMEOUT: Duration = Duration::from_secs(3);
pub const STREAM_OPEN_TIMEOUT: Duration = Duration::from_secs(2);
/// Streamed request body cap (plan §3).
pub const MAX_BODY: u64 = 32 * 1024 * 1024;
/// The editor must send its request head within this time (slow-loris guard).
pub const HEAD_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct UseConfig {
    /// Pinned share address, must end in `/p2p/<PeerId>`.
    pub peer: Multiaddr,
    /// Local editor address; must be loopback.
    pub bind: SocketAddr,
    pub paired: PairedConfig,
    /// Keep one pre-opened spare stream (default on; off only for A/B measurements).
    pub spare_stream: bool,
    pub verbose: bool,
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
    pub cancels_sent: u64,
    pub peer_lost_mid_request: u64,
}

pub type SharedUseStats = Arc<Mutex<UseStats>>;

pub struct UseHandle {
    pub peer_id: PeerId,
    pub bind: SocketAddr,
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
}

type Channel = (ChannelReader, ChannelWriter);

struct Spare {
    generation: u64,
    channel: Channel,
}

struct Shared {
    local: PeerId,
    peer: PeerId,
    control: Mutex<Control>,
    /// `Some(generation)` while connected; generation increments per connection.
    connection: watch::Receiver<Option<u64>>,
    spare: Mutex<Option<Spare>>,
    spare_wanted: Notify,
    spare_enabled: bool,
    stats: SharedUseStats,
    verbose: bool,
}

fn other<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(e.to_string())
}

impl Shared {
    async fn open(&self, protocol: libp2p::StreamProtocol) -> io::Result<Channel> {
        let mut control = self.control.lock().unwrap().clone();
        let t0 = Instant::now();
        let stream = tokio::time::timeout(STREAM_OPEN_TIMEOUT, control.open_stream(self.peer, protocol.clone()))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "stream open timed out"))?
            .map_err(other)?;
        let context = channel_context(&self.local, &self.peer, &protocol);
        let channel = tokio::time::timeout(
            STREAM_OPEN_TIMEOUT,
            open_channel(stream, StreamRole::Initiator, &context),
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
        writer.send_json(FrameType::StatusReq, &req).await?;
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
    let listener = TcpListener::bind(cfg.bind).await?;
    let bind = listener.local_addr()?;

    let mut swarm = build_paired_swarm(identity, &cfg.paired);
    let local = *swarm.local_peer_id();
    swarm.add_peer_address(peer, cfg.peer.clone());
    swarm.dial(cfg.peer.clone()).map_err(other)?;
    let started = Instant::now();
    tokio::time::timeout(DIAL_TIMEOUT, async {
        loop {
            match swarm.select_next_some().await {
                SwarmEvent::ConnectionEstablished { peer_id, .. } if peer_id == peer => return Ok(()),
                SwarmEvent::OutgoingConnectionError { error, .. } => return Err(other(error)),
                _ => {}
            }
        }
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "dial timed out (is the other Mac awake and sharing?)"))??;
    eprintln!(
        "use event=connected peer={peer} dial_ms={:.2} unix_ms={}",
        started.elapsed().as_secs_f64() * 1e3,
        unix_ms()
    );
    let stats = SharedUseStats::default();
    stats.lock().unwrap().connections_established += 1;
    let (conn_tx, conn_rx) = watch::channel(Some(1u64));
    let control = swarm.behaviour().stream.new_control();
    let shared = Arc::new(Shared {
        local,
        peer,
        control: Mutex::new(control),
        connection: conn_rx,
        spare: Mutex::new(None),
        spare_wanted: Notify::new(),
        spare_enabled: cfg.spare_stream,
        stats: stats.clone(),
        verbose: cfg.verbose,
    });

    let swarm_stats = stats.clone();
    let verbose = cfg.verbose;
    // No behaviour stores peer addresses, so re-dial with the full pinned multiaddr.
    let redial_addr = cfg.peer.clone();
    let swarm_task = tokio::spawn(async move {
        let mut generation = 1u64;
        let mut open_conns = 1usize;
        let mut redial = tokio::time::interval(Duration::from_secs(2));
        redial.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
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
                    SwarmEvent::ConnectionEstablished { peer_id, .. } if peer_id == peer => {
                        open_conns += 1;
                        if open_conns == 1 {
                            generation += 1;
                            swarm_stats.lock().unwrap().connections_established += 1;
                            let _ = conn_tx.send(Some(generation));
                            eprintln!("use event=connected peer={peer_id} unix_ms={}", unix_ms());
                        }
                    }
                    SwarmEvent::ConnectionClosed { peer_id, cause, num_established, .. } if peer_id == peer => {
                        open_conns = num_established as usize;
                        if num_established == 0 {
                            swarm_stats.lock().unwrap().connections_closed += 1;
                            let _ = conn_tx.send(None);
                            eprintln!("use event=closed peer={peer_id} cause={cause:?} unix_ms={}", unix_ms());
                        }
                    }
                    _ => {}
                },
                _ = redial.tick() => {
                    if open_conns == 0 && !swarm.is_connected(&peer) {
                        if let Err(e) = swarm.dial(redial_addr.clone()) {
                            if verbose {
                                eprintln!("use event=redial_error error={e}");
                            }
                        }
                    }
                }
            }
        }
    });

    let mut tasks = vec![swarm_task];
    if cfg.spare_stream {
        tasks.push(tokio::spawn(spare_refill(shared.clone())));
    }
    let accept_shared = shared.clone();
    tasks.push(tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            };
            let shared = accept_shared.clone();
            tokio::spawn(async move {
                if let Err(e) = serve_connection(&shared, tcp, bind.port()).await {
                    if shared.verbose {
                        eprintln!("use event=request_error error={e}");
                    }
                }
            });
        }
    }));
    eprintln!("use event=serving bind=http://{bind}/v1 peer={peer}");
    Ok(UseHandle {
        peer_id: peer,
        bind,
        stats,
        shared,
        tasks,
    })
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
    if allowlist::browser_origin_refused(head.header("origin"), head.header("sec-fetch-site")) {
        return refuse(shared, &mut w, &http::forbidden()).await;
    }
    if allowlist::route(&head.method, &head.target).is_none() {
        return refuse(shared, &mut w, &http::forbidden()).await;
    }
    let kind = match body_kind(&head) {
        Ok(k) => k,
        Err(msg) => return refuse(shared, &mut w, &http::error_response(400, "bad_request", msg)).await,
    };
    if let BodyKind::Length(n) = kind {
        if n > MAX_BODY {
            return refuse(
                shared,
                &mut w,
                &http::error_response(413, "body_too_large", "Request body larger than 32 MiB."),
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
    shared.stats.lock().unwrap().requests += 1;

    let (mut reader, mut writer) = match shared.take_channel().await {
        Ok(c) => c,
        Err(e) => {
            return reply(
                &mut w,
                &http::error_response(
                    502,
                    "peer_unreachable",
                    &format!("The other Mac is not reachable ({e})."),
                ),
            )
            .await
        }
    };
    let req = ReqHead {
        method: head.method.clone(),
        target: head.target.clone(),
        headers: allowlist::filter_request_headers(&head.headers),
        body: kind,
    };
    if let Err(e) = writer.send_json(FrameType::ReqHead, &req).await {
        return reply(
            &mut w,
            &http::error_response(502, "peer_unreachable", &format!("The other Mac is not reachable ({e}).")),
        )
        .await;
    }

    enum End {
        ResponseDone(io::Result<()>),
        ClientGone,
        BodyRefused(&'static str),
    }
    let mut state = RespState::default();
    let end = {
        let up = upload_then_watch(&mut r, early_body, kind, &mut writer);
        let down = relay_response(&mut reader, &mut w, &mut state);
        tokio::pin!(up, down);
        tokio::select! {
            res = &mut down => End::ResponseDone(res),
            u = &mut up => match u {
                Upload::ClientGone => End::ClientGone,
                Upload::TooLarge => End::BodyRefused("Request body larger than 32 MiB."),
                Upload::Malformed => End::BodyRefused("Malformed chunked request body."),
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
                if state.chunked && state.event_stream {
                    let event = format!(
                        "data: {}\n\n",
                        String::from_utf8_lossy(&http::error_body(
                            502,
                            "server_error",
                            "peer_lost",
                            "The other Mac stopped answering.",
                        ))
                    );
                    let chunk = format!("{:x}\r\n{event}\r\n", event.len());
                    let _ = w.write_all(chunk.as_bytes()).await;
                    let _ = w.flush().await;
                }
            } else {
                let _ = w
                    .write_all(&http::error_response(
                        502,
                        "peer_lost",
                        &format!("The other Mac stopped answering ({e})."),
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
                let status = if msg.contains("32 MiB") { 413 } else { 400 };
                let _ = reply(&mut w, &http::error_response(status, "bad_request", msg)).await;
            }
        }
    }
    Ok(())
}

#[derive(Debug, Default, Clone)]
struct RespState {
    head_sent: bool,
    chunked: bool,
    event_stream: bool,
}

enum Upload {
    ClientGone,
    TooLarge,
    Malformed,
}

/// Stream the request body as ReqBody frames (cap 32 MiB), send the end marker, then
/// watch the editor's socket: EOF/error = the editor went away (stop button).
async fn upload_then_watch(
    r: &mut OwnedReadHalf,
    early: Vec<u8>,
    kind: BodyKind,
    writer: &mut ChannelWriter,
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
                match r.read(&mut buf[..want]).await {
                    Ok(0) | Err(_) => return Upload::ClientGone,
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
                match r.read(&mut buf).await {
                    Ok(0) | Err(_) => return Upload::ClientGone,
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
                }
                w.write_all(&frame.payload).await?;
                w.flush().await?;
                state.head_sent = true;
            }
            FrameType::RespBody if state.head_sent => {
                if frame.payload.is_empty() {
                    return Ok(());
                }
                w.write_all(&frame.payload).await?;
                w.flush().await?;
            }
            FrameType::Error if !state.head_sent => {
                let err: WireError = serde_json::from_slice(&frame.payload)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                w.write_all(&http::error_response(err.status, &err.code, &err.message))
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

#[derive(Default, Clone, Copy, PartialEq, Eq)]
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
    }
}
