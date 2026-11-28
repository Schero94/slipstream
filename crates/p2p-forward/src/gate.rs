//! Share-side swarm loop with the M2 trust gate (plan §2 "Gate at ConnectionEstablished
//! … and again per stream"). Used by [`crate::worker::start_share`] and by the headless
//! pairing listener (`slipstream-node pair --show-code`).
//!
//! - `ConnectionEstablished`: trusted → kept; untrusted → closed at once, unless a pairing
//!   window is open (then kept as a *guest*; only `/slipstream/pair/1` will serve it,
//!   every other acceptor re-checks [`PeerTrust::trusted_key`] per stream);
//! - trust change (store reload, window closed) and every [`GATE_TICK`]: peers that lost
//!   trust get a best-effort `notify_revoked` (≤ [`NOTIFY_REVOKED_BUDGET`]) and are then
//!   disconnected; guests are disconnected as soon as no window is open.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::time::Instant;

use futures::StreamExt;
use libp2p::swarm::SwarmEvent;
use libp2p::{Multiaddr, PeerId, Swarm};
use p2p_libp2p::paired::{build_paired_swarm, handle_ping, Control, PairedBehaviour, PairedConfig, PairedEvent};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::trust::{PeerTrust, GATE_TICK, NOTIFY_REVOKED_BUDGET};
use crate::unix_ms;
use crate::worker::{LogRateLimit, SharedShareStats};

const REJECT_LOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

/// A listening paired swarm driven by the gate loop.
pub struct GatedListener {
    pub peer_id: PeerId,
    pub listen_addr: Multiaddr,
    pub control: Control,
    pub task: JoinHandle<()>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeerState {
    Trusted([u8; 32]),
    /// Untrusted, admitted while a pairing window is open (pair protocol only).
    Guest,
    /// Lost trust; goodbye in flight, disconnect follows.
    Revoking,
}

pub async fn start_gated_listener(
    identity: libp2p::identity::Keypair,
    listen: Multiaddr,
    paired: &PairedConfig,
    trust: Arc<dyn PeerTrust>,
    stats: SharedShareStats,
    role: &'static str,
    verbose: bool,
) -> io::Result<GatedListener> {
    let mut swarm = build_paired_swarm(identity, paired);
    let peer_id = *swarm.local_peer_id();
    swarm.listen_on(listen).map_err(|e| io::Error::other(e.to_string()))?;
    let listen_addr = loop {
        if let SwarmEvent::NewListenAddr { address, .. } = swarm.select_next_some().await {
            break address;
        }
    };
    let control = swarm.behaviour().stream.new_control();
    let task = tokio::spawn(gate_loop(swarm, control.clone(), trust, stats, role, verbose));
    Ok(GatedListener {
        peer_id,
        listen_addr,
        control,
        task,
    })
}

async fn gate_loop(
    mut swarm: Swarm<PairedBehaviour>,
    control: Control,
    trust: Arc<dyn PeerTrust>,
    stats: SharedShareStats,
    role: &'static str,
    verbose: bool,
) {
    let mut reject_log = LogRateLimit::new(REJECT_LOG_INTERVAL);
    let mut epoch = trust.subscribe();
    let mut epoch_live = true;
    let mut tick = tokio::time::interval(GATE_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let (close_tx, mut close_rx) = mpsc::unbounded_channel::<PeerId>();
    let mut peers: HashMap<PeerId, PeerState> = HashMap::new();
    loop {
        let reevaluate = tokio::select! {
            event = swarm.select_next_some() => {
                match event {
                    SwarmEvent::Behaviour(PairedEvent::Ping(event)) => {
                        if let Some(rtt) = handle_ping(&mut swarm, &event) {
                            if verbose {
                                eprintln!("{role} ping peer={} rtt_us={}", event.peer, rtt.as_micros());
                            }
                        }
                    }
                    SwarmEvent::ConnectionEstablished { peer_id, connection_id, .. } => {
                        match trust.trusted_key(&peer_id) {
                            Some(key) => {
                                peers.insert(peer_id, PeerState::Trusted(key));
                                stats.lock().unwrap().connections_established += 1;
                                eprintln!("{role} event=connected peer={peer_id} unix_ms={}", unix_ms());
                            }
                            None if trust.pairing_open() => {
                                peers.entry(peer_id).or_insert(PeerState::Guest);
                                stats.lock().unwrap().pairing_guests += 1;
                                eprintln!("{role} event=pairing_guest peer={peer_id} unix_ms={}", unix_ms());
                            }
                            None => {
                                // Trust gate: closed before any stream of theirs is read.
                                stats.lock().unwrap().connections_rejected += 1;
                                if let Some(suppressed) = reject_log.allow(Instant::now()) {
                                    eprintln!(
                                        "{role} event=rejected_peer peer={peer_id} suppressed_since_last={suppressed} unix_ms={}",
                                        unix_ms()
                                    );
                                }
                                swarm.close_connection(connection_id);
                            }
                        }
                    }
                    SwarmEvent::ConnectionClosed { peer_id, cause, num_established, .. } => {
                        let was = peers.get(&peer_id).copied();
                        if num_established == 0 {
                            peers.remove(&peer_id);
                        }
                        if matches!(was, Some(PeerState::Trusted(_)) | Some(PeerState::Revoking)) {
                            stats.lock().unwrap().connections_closed += 1;
                            eprintln!("{role} event=closed peer={peer_id} cause={cause:?} unix_ms={}", unix_ms());
                        }
                    }
                    SwarmEvent::IncomingConnectionError { error, .. } => {
                        if verbose {
                            eprintln!("{role} event=incoming_error error={error}");
                        }
                    }
                    _ => {}
                }
                false
            }
            changed = epoch.changed(), if epoch_live => {
                if changed.is_err() {
                    epoch_live = false;
                }
                true
            }
            _ = tick.tick() => true,
            Some(peer) = close_rx.recv() => {
                if trust.trusted_key(&peer).is_none() {
                    let _ = swarm.disconnect_peer_id(peer);
                    stats.lock().unwrap().revoked_disconnects += 1;
                    eprintln!("{role} event=revoked_disconnect peer={peer} unix_ms={}", unix_ms());
                } else if let Some(key) = trust.trusted_key(&peer) {
                    peers.insert(peer, PeerState::Trusted(key));
                }
                false
            }
        };
        if !reevaluate {
            continue;
        }
        let open = trust.pairing_open();
        let connected: Vec<PeerId> = swarm.connected_peers().copied().collect();
        for peer in connected {
            let state = peers.get(&peer).copied().unwrap_or(PeerState::Guest);
            match (state, trust.trusted_key(&peer)) {
                (PeerState::Revoking, _) => {}
                (_, Some(key)) => {
                    if state != PeerState::Trusted(key) {
                        peers.insert(peer, PeerState::Trusted(key));
                    }
                }
                (PeerState::Trusted(old), None) => {
                    peers.insert(peer, PeerState::Revoking);
                    eprintln!("{role} event=revoked peer={peer} unix_ms={}", unix_ms());
                    let notify = trust.notify_revoked(peer, old, control.clone());
                    let close_tx = close_tx.clone();
                    tokio::spawn(async move {
                        let _ = tokio::time::timeout(NOTIFY_REVOKED_BUDGET, notify).await;
                        let _ = close_tx.send(peer);
                    });
                }
                (PeerState::Guest, None) => {
                    if !open {
                        let _ = swarm.disconnect_peer_id(peer);
                        stats.lock().unwrap().guests_closed += 1;
                        if verbose {
                            eprintln!("{role} event=guest_closed peer={peer}");
                        }
                    }
                }
            }
        }
    }
}
