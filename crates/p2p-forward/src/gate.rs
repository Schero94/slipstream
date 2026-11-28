//! Share-side swarm loop with the M2 trust gate (plan §2 "Gate at ConnectionEstablished
//! … and again per stream"). Used by [`crate::worker::start_share`] and by the headless
//! pairing listener (`slipstream-node pair --show-code`).
//!
//! - `ConnectionEstablished`: trusted **in the listener's role** (security review M1: the
//!   share side admits only Macs paired as users of this one) → kept; anything else → closed
//!   at once, unless a pairing window is open (then kept as a *guest*; only
//!   `/slipstream/pair/1` will serve it, every other acceptor re-checks
//!   [`PeerTrust::trusted_key_for`] per stream). Guests have their own small cap
//!   ([`MAX_GUEST_CONNECTIONS`], review M5); the swarm's connection limit has room for them
//!   on top of the trusted peers' slots, so guests cannot crowd out paired Macs;
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

use crate::trust::{PeerTrust, Role, FORMER_UNPAIR_DELAY, FORMER_UNPAIR_EVERY, GATE_TICK, NOTIFY_REVOKED_BUDGET};
use crate::unix_ms;
use crate::worker::{LogRateLimit, SharedShareStats};

const REJECT_LOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

/// Guest (untrusted, pairing-window) connections kept at once (security review M5). Further
/// guest connections are closed at `ConnectionEstablished`; the gated swarm's total limit is
/// `max_established_total + MAX_GUEST_CONNECTIONS`, so guests never use the trusted peers' slots.
pub const MAX_GUEST_CONNECTIONS: usize = 4;
/// Guest connections kept per source IP address (M5 per-source limit): one address on the
/// LAN cannot take every guest place and lock the real Mac out of pairing.
pub const MAX_GUESTS_PER_IP: usize = 2;

fn remote_ip(endpoint: &libp2p::core::ConnectedPoint) -> Option<std::net::IpAddr> {
    use libp2p::multiaddr::Protocol;
    endpoint.get_remote_address().iter().find_map(|p| match p {
        Protocol::Ip4(v) => Some(std::net::IpAddr::V4(v)),
        Protocol::Ip6(v) => Some(std::net::IpAddr::V6(v)),
        _ => None,
    })
}

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
    admit: Role,
    verbose: bool,
) -> io::Result<GatedListener> {
    let mut paired = paired.clone();
    // Room for the guests on top of the trusted peers' slots (M5).
    paired.max_established_total = paired.max_established_total.saturating_add(MAX_GUEST_CONNECTIONS as u32);
    let mut swarm = build_paired_swarm(identity, &paired);
    let peer_id = *swarm.local_peer_id();
    swarm.listen_on(listen).map_err(|e| io::Error::other(e.to_string()))?;
    let listen_addr = loop {
        if let SwarmEvent::NewListenAddr { address, .. } = swarm.select_next_some().await {
            break address;
        }
    };
    let control = swarm.behaviour().stream.new_control();
    let task = tokio::spawn(gate_loop(swarm, control.clone(), trust, stats, role, admit, verbose));
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
    admit: Role,
    verbose: bool,
) {
    let mut reject_log = LogRateLimit::new(REJECT_LOG_INTERVAL);
    let mut epoch = trust.subscribe();
    let mut epoch_live = true;
    let mut tick = tokio::time::interval(GATE_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let (close_tx, mut close_rx) = mpsc::unbounded_channel::<PeerId>();
    let mut peers: HashMap<PeerId, PeerState> = HashMap::new();
    // Guest connections currently kept (M5 cap).
    let mut guest_conns: HashMap<libp2p::swarm::ConnectionId, Option<std::net::IpAddr>> = HashMap::new();
    // M5: last `Unpair` answer per removed Mac (rate limit).
    let mut former_sent: HashMap<PeerId, Instant> = HashMap::new();
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
                    SwarmEvent::ConnectionEstablished { peer_id, connection_id, endpoint, .. } => {
                        let ip = remote_ip(&endpoint);
                        let guests_full = guest_conns.len() >= MAX_GUEST_CONNECTIONS
                            || guest_conns.values().filter(|g| **g == ip).count() >= MAX_GUESTS_PER_IP;
                        match trust.trusted_key_for(&peer_id, admit) {
                            Some(key) => {
                                peers.insert(peer_id, PeerState::Trusted(key));
                                stats.lock().unwrap().connections_established += 1;
                                eprintln!("{role} event=connected peer={peer_id} unix_ms={}", unix_ms());
                            }
                            None if trust.pairing_open() && guests_full => {
                                stats.lock().unwrap().guests_refused += 1;
                                if verbose {
                                    eprintln!("{role} event=guest_refused peer={peer_id} reason=guest_cap");
                                }
                                swarm.close_connection(connection_id);
                            }
                            None if trust.pairing_open() => {
                                guest_conns.insert(connection_id, ip);
                                peers.entry(peer_id).or_insert(PeerState::Guest);
                                stats.lock().unwrap().pairing_guests += 1;
                                eprintln!("{role} event=pairing_guest peer={peer_id} unix_ms={}", unix_ms());
                            }
                            None if trust.former_key(&peer_id).is_some()
                                && former_sent.get(&peer_id).is_none_or(|t| t.elapsed() >= FORMER_UNPAIR_EVERY) =>
                            {
                                // A Mac removed while it was offline: tell it once (rate-limited)
                                // with an authenticated `Unpair` under its old key, then close.
                                // It never gets a forwarder stream (those re-check trust).
                                let key = trust.former_key(&peer_id).expect("checked");
                                former_sent.insert(peer_id, Instant::now());
                                peers.insert(peer_id, PeerState::Revoking);
                                stats.lock().unwrap().connections_rejected += 1;
                                stats.lock().unwrap().former_unpairs += 1;
                                eprintln!("{role} event=former_peer_unpair peer={peer_id} unix_ms={}", unix_ms());
                                let notify = trust.notify_revoked(peer_id, key, control.clone());
                                let close_tx = close_tx.clone();
                                tokio::spawn(async move {
                                    tokio::time::sleep(FORMER_UNPAIR_DELAY).await;
                                    let _ = tokio::time::timeout(NOTIFY_REVOKED_BUDGET, notify).await;
                                    let _ = close_tx.send(peer_id);
                                });
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
                    SwarmEvent::ConnectionClosed { peer_id, connection_id, cause, num_established, .. } => {
                        guest_conns.remove(&connection_id);
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
                if trust.trusted_key_for(&peer, admit).is_none() {
                    let _ = swarm.disconnect_peer_id(peer);
                    stats.lock().unwrap().revoked_disconnects += 1;
                    eprintln!("{role} event=revoked_disconnect peer={peer} unix_ms={}", unix_ms());
                } else if let Some(key) = trust.trusted_key_for(&peer, admit) {
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
            match (state, trust.trusted_key_for(&peer, admit)) {
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
