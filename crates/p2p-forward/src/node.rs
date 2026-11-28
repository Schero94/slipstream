//! Share side (big Mac) and use side (MacBook) of the M0 raw pipe.
//!
//! share: accepts `/slipstream/pipe/0` streams on its paired QUIC swarm and pipes each
//!        one to a fresh TCP connection to `upstream` (llama-server or the mock).
//! use:   dials the pinned peer once (persistent connection), binds a loopback TCP
//!        listener, and pipes each accepted TCP connection over a new stream.

use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use libp2p::multiaddr::Protocol;
use libp2p::swarm::SwarmEvent;
use libp2p::{Multiaddr, PeerId, Swarm};
use p2p_crypto::StreamRole;
use p2p_libp2p::paired::{
    build_paired_swarm, handle_ping, PairedBehaviour, PairedConfig, PairedEvent, PIPE_PROTOCOL_V0,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::pipe::{pipe, stream_context, PipeMode};

pub const DIAL_TIMEOUT: Duration = Duration::from_secs(3);
pub const STREAM_OPEN_TIMEOUT: Duration = Duration::from_secs(2);

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Observations shared with tests and the bench harness.
#[derive(Debug, Default)]
pub struct NodeStats {
    pub ping_rtt_us: Vec<u64>,
    pub stream_open_us: Vec<u64>,
    pub connections_established: u64,
    pub connections_closed: u64,
    pub streams_piped: u64,
}

pub type SharedStats = Arc<Mutex<NodeStats>>;

fn other<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(e.to_string())
}

/// Drive the swarm forever: ping policy, connection logging.
fn spawn_swarm_loop(
    mut swarm: Swarm<PairedBehaviour>,
    stats: SharedStats,
    role: &'static str,
    verbose: bool,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match swarm.select_next_some().await {
                SwarmEvent::Behaviour(PairedEvent::Ping(event)) => {
                    if let Some(rtt) = handle_ping(&mut swarm, &event) {
                        stats
                            .lock()
                            .unwrap()
                            .ping_rtt_us
                            .push(rtt.as_micros() as u64);
                        if verbose {
                            eprintln!("{role} ping peer={} rtt_us={}", event.peer, rtt.as_micros());
                        }
                    } else {
                        eprintln!(
                            "{role} event=ping_failed peer={} unix_ms={}",
                            event.peer,
                            unix_ms()
                        );
                    }
                }
                SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                    stats.lock().unwrap().connections_established += 1;
                    eprintln!(
                        "{role} event=connected peer={peer_id} unix_ms={}",
                        unix_ms()
                    );
                }
                SwarmEvent::ConnectionClosed { peer_id, cause, .. } => {
                    stats.lock().unwrap().connections_closed += 1;
                    eprintln!(
                        "{role} event=closed peer={peer_id} cause={cause:?} unix_ms={}",
                        unix_ms()
                    );
                }
                SwarmEvent::IncomingConnectionError { error, .. } => {
                    eprintln!("{role} event=incoming_error error={error}");
                }
                _ => {}
            }
        }
    })
}

pub struct ShareHandle {
    pub peer_id: PeerId,
    pub listen_addr: Multiaddr,
    pub stats: SharedStats,
    tasks: Vec<JoinHandle<()>>,
}

impl ShareHandle {
    /// Full pinned address for the use side: `<listen>/p2p/<peer_id>`.
    pub fn dial_addr(&self) -> Multiaddr {
        self.listen_addr.clone().with(Protocol::P2p(self.peer_id))
    }
}

impl Drop for ShareHandle {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

pub async fn start_share(
    identity: libp2p::identity::Keypair,
    listen: Multiaddr,
    upstream: SocketAddr,
    mode: PipeMode,
    config: &PairedConfig,
    verbose: bool,
) -> io::Result<ShareHandle> {
    let mut swarm = build_paired_swarm(identity, config);
    let local = *swarm.local_peer_id();
    swarm.listen_on(listen).map_err(other)?;
    let listen_addr = loop {
        if let SwarmEvent::NewListenAddr { address, .. } = swarm.select_next_some().await {
            break address;
        }
    };
    let mut control = swarm.behaviour().stream.new_control();
    let mut incoming = control.accept(PIPE_PROTOCOL_V0).map_err(other)?;
    let stats = SharedStats::default();
    let swarm_task = spawn_swarm_loop(swarm, stats.clone(), "share", verbose);
    let accept_stats = stats.clone();
    let accept_task = tokio::spawn(async move {
        while let Some((peer, stream)) = incoming.next().await {
            let stats = accept_stats.clone();
            tokio::spawn(async move {
                let result = async {
                    let tcp = TcpStream::connect(upstream).await?;
                    let ctx = stream_context(&peer, &local);
                    pipe(tcp, stream, mode, StreamRole::Responder, &ctx).await
                }
                .await;
                stats.lock().unwrap().streams_piped += 1;
                if let Err(e) = result {
                    eprintln!("share event=pipe_error peer={peer} error={e}");
                }
            });
        }
    });
    Ok(ShareHandle {
        peer_id: local,
        listen_addr,
        stats,
        tasks: vec![swarm_task, accept_task],
    })
}

pub struct UseHandle {
    pub peer_id: PeerId,
    pub bind: SocketAddr,
    pub stats: SharedStats,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for UseHandle {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// Dial the pinned peer (`.../p2p/<id>` required), keep the connection, serve `bind`.
pub async fn start_use(
    identity: libp2p::identity::Keypair,
    peer_addr: Multiaddr,
    bind: SocketAddr,
    mode: PipeMode,
    config: &PairedConfig,
    verbose: bool,
) -> io::Result<UseHandle> {
    let Some(Protocol::P2p(peer)) = peer_addr.iter().last() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "peer address must end in /p2p/<peer id> (pinned dialing only)",
        ));
    };
    let mut swarm = build_paired_swarm(identity, config);
    let local = *swarm.local_peer_id();
    swarm.add_peer_address(peer, peer_addr.clone());
    swarm.dial(peer_addr).map_err(other)?;
    let started = Instant::now();
    tokio::time::timeout(DIAL_TIMEOUT, async {
        loop {
            match swarm.select_next_some().await {
                SwarmEvent::ConnectionEstablished { peer_id, .. } if peer_id == peer => {
                    return Ok(());
                }
                SwarmEvent::OutgoingConnectionError { error, .. } => return Err(other(error)),
                _ => {}
            }
        }
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "dial timed out"))??;
    eprintln!(
        "use event=connected peer={peer} dial_ms={:.2} unix_ms={}",
        started.elapsed().as_secs_f64() * 1e3,
        unix_ms()
    );
    let stats = SharedStats::default();
    stats.lock().unwrap().connections_established += 1;
    let control = swarm.behaviour().stream.new_control();
    let swarm_task = spawn_swarm_loop(swarm, stats.clone(), "use", verbose);
    let listener = TcpListener::bind(bind).await?;
    let bind = listener.local_addr()?;
    let accept_stats = stats.clone();
    let accept_task = tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                continue;
            };
            let mut control = control.clone();
            let stats = accept_stats.clone();
            tokio::spawn(async move {
                let t0 = Instant::now();
                let opened = tokio::time::timeout(
                    STREAM_OPEN_TIMEOUT,
                    control.open_stream(peer, PIPE_PROTOCOL_V0),
                )
                .await;
                let stream = match opened {
                    Ok(Ok(stream)) => stream,
                    Ok(Err(e)) => {
                        eprintln!("use event=open_error error={e}");
                        return;
                    }
                    Err(_) => {
                        eprintln!("use event=open_timeout");
                        return;
                    }
                };
                let open_us = t0.elapsed().as_micros() as u64;
                stats.lock().unwrap().stream_open_us.push(open_us);
                if verbose {
                    eprintln!("use stream_open_us={open_us}");
                }
                let ctx = stream_context(&local, &peer);
                if let Err(e) = pipe(tcp, stream, mode, StreamRole::Initiator, &ctx).await {
                    eprintln!("use event=pipe_error error={e}");
                }
                stats.lock().unwrap().streams_piped += 1;
            });
        }
    });
    Ok(UseHandle {
        peer_id: peer,
        bind,
        stats,
        tasks: vec![swarm_task, accept_task],
    })
}
