//! LAN discovery glue (v0.6 M3, docs/plans/2026-11-28-use-your-other-mac.md §1 / §5).
//!
//! - Share side: [`start_share_announcer`] announces `_slipstream._udp` while sharing (or
//!   while a standalone pairing window listens) and keeps `pair=1` in step with the
//!   pairing window.
//! - Use side: [`start_use_discovery`] browses continuously, matches announced
//!   fingerprints against the **pinned** PeerId from the trust store and feeds the
//!   forwarder's [`AddrBook`] (announced addresses + `last_addrs` fallback), always with the
//!   pinned `/p2p/<PeerId>` suffix. mDNS data is untrusted: an impostor announcing our
//!   peer's fingerprint only gets dialed *as the pinned PeerId*, which its QUIC TLS
//!   handshake cannot prove. Successful connections update `last_addrs`; dial failures and
//!   disconnects trigger a re-browse; the browser itself re-browses when the interface
//!   set changes (polled every 5 s).
//! - CLI: [`discover`] lists sharing Macs for `slipstream-node discover` and
//!   [`resolve_fp`] turns a fingerprint into pairing candidates for `pair --peer <fp>`.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, PeerId};
use p2p_forward::client::pinned_candidates;
use p2p_forward::trust::PeerTrust;
use p2p_forward::{AddrBook, UseDiscovery, UseEvent};
use p2p_net::discovery::mdns::{self, DiscoveryEvent, ShareAdvert, ShareHandle, ShareProto};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::trust::{NodeTrust, TrustStore, TrustedPeer};

/// Extra listening time after `pair --peer <fp>` found the Mac (more address records).
pub const ADDR_SETTLE: Duration = Duration::from_millis(500);

/// How often the announcer checks the pairing window (it can expire without an event).
pub const WINDOW_POLL: Duration = Duration::from_millis(200);

/// The TXT `fp`: first 8 bytes of SHA-256(PeerId bytes), 16 lowercase hex chars.
pub fn fingerprint(peer: &PeerId) -> String {
    let digest = Sha256::digest(peer.to_bytes());
    hex::encode(&digest[..8])
}

/// `/ip4|ip6/<ip>/udp/<port>/quic-v1/p2p/<peer>` for each announced IP.
pub fn pinned_addrs(peer: PeerId, ips: &[IpAddr], port: u16) -> Vec<Multiaddr> {
    let mut v4: Vec<Multiaddr> = Vec::new();
    let mut v6: Vec<Multiaddr> = Vec::new();
    for ip in ips {
        let a = Multiaddr::empty()
            .with(match ip {
                IpAddr::V4(v) => Protocol::Ip4(*v),
                IpAddr::V6(v) => Protocol::Ip6(*v),
            })
            .with(Protocol::Udp(port))
            .with(Protocol::QuicV1)
            .with(Protocol::P2p(peer));
        if ip.is_ipv4() {
            v4.push(a)
        } else {
            v6.push(a)
        }
    }
    v4.extend(v6);
    v4
}

/// Announce only listeners other Macs can reach: an unspecified (`0.0.0.0` / `::`) or a
/// non-loopback IP. A loopback listener (tests, local tools) is never announced.
pub fn should_announce(listen: &Multiaddr) -> bool {
    match listen.iter().next() {
        Some(Protocol::Ip4(ip)) => !ip.is_loopback(),
        Some(Protocol::Ip6(ip)) => !ip.is_loopback(),
        _ => false,
    }
}

pub fn udp_port(addr: &Multiaddr) -> Option<u16> {
    addr.iter().find_map(|p| match p {
        Protocol::Udp(port) => Some(port),
        _ => None,
    })
}

/// A running announcement; dropping it removes it (mDNS goodbye).
pub struct ShareAnnouncer {
    pub handle: Arc<ShareHandle>,
    task: JoinHandle<()>,
}

impl Drop for ShareAnnouncer {
    fn drop(&mut self) {
        self.task.abort();
        eprintln!(
            "lan event=unannouncing strong_refs={} unix_ms={}",
            Arc::strong_count(&self.handle),
            p2p_forward::unix_ms()
        );
    }
}

/// Announce `peer` on `port`; `pair=1` follows `trust.pairing_open()`.
pub fn start_share_announcer(peer: PeerId, port: u16, trust: Arc<NodeTrust>) -> Result<ShareAnnouncer, String> {
    let open = trust.pairing_open();
    let handle = Arc::new(
        mdns::announce_share(&ShareAdvert {
            fp: fingerprint(&peer),
            proto: ShareProto::QuicV1,
            pairing_open: open,
            port,
        })
        .map_err(|e| e.to_string())?,
    );
    eprintln!(
        "lan event=announced fp={} instance={} port={port} pair={}",
        fingerprint(&peer),
        handle.fullname(),
        u8::from(open)
    );
    // Weak: dropping the announcer must drop the last strong ref at once (unregister +
    // goodbye), not whenever the aborted task is finally dropped.
    let h = Arc::downgrade(&handle);
    let task = tokio::spawn(async move {
        let mut last = open;
        let mut epoch = trust.subscribe();
        loop {
            tokio::select! {
                _ = tokio::time::sleep(WINDOW_POLL) => {}
                r = epoch.changed() => if r.is_err() { tokio::time::sleep(WINDOW_POLL).await },
            }
            let now = trust.pairing_open();
            if now != last {
                last = now;
                let Some(h) = h.upgrade() else { return };
                match h.set_pairing_open(now) {
                    Ok(()) => eprintln!("lan event=pairing_flag pair={}", u8::from(now)),
                    Err(e) => eprintln!("lan event=pairing_flag_error error={e}"),
                }
            }
        }
    });
    Ok(ShareAnnouncer { handle, task })
}

/// Use-side discovery task; dropping it stops browsing.
pub struct UseLan {
    task: JoinHandle<()>,
}

impl Drop for UseLan {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn known_addrs(store: &TrustStore, peer: &PeerId) -> Vec<Multiaddr> {
    let entry = store.get(peer);
    let parsed: Vec<Multiaddr> = entry
        .map(|e| e.last_addrs.iter().filter_map(|a| a.parse().ok()).collect())
        .unwrap_or_default();
    pinned_candidates(*peer, parsed.iter())
}

/// Start browsing for the pinned `peer` and return the forwarder's discovery inputs.
pub fn start_use_discovery(store: Arc<TrustStore>, peer: PeerId, verbose: bool) -> Result<(UseDiscovery, UseLan), String> {
    let mut discovery = mdns::browse_continuous().map_err(|e| e.to_string())?;
    let fp = fingerprint(&peer);
    let (book_tx, book_rx) = watch::channel(AddrBook {
        discovered: Vec::new(),
        known: known_addrs(&store, &peer),
    });
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<UseEvent>();
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                ev = discovery.recv() => match ev {
                    Some(DiscoveryEvent::Found { fp: f, addrs, port, .. }) if f == fp => {
                        let discovered = pinned_addrs(peer, &addrs, port);
                        eprintln!("lan event=found fp={f} peer={peer} addrs={} port={port} unix_ms={}", addrs.len(), p2p_forward::unix_ms());
                        book_tx.send_if_modified(|b| {
                            let changed = b.discovered != discovered;
                            b.discovered = discovered;
                            changed
                        });
                    }
                    Some(DiscoveryEvent::Lost { fp: f }) if f == fp => {
                        eprintln!("lan event=lost fp={f} unix_ms={}", p2p_forward::unix_ms());
                        book_tx.send_if_modified(|b| {
                            let changed = !b.discovered.is_empty();
                            b.discovered.clear();
                            changed
                        });
                    }
                    Some(other) => {
                        if verbose {
                            eprintln!("lan event=ignored {other:?}");
                        }
                    }
                    None => return,
                },
                ev = ev_rx.recv() => match ev {
                    Some(UseEvent::Connected { addr }) => {
                        let store = store.clone();
                        let known = tokio::task::spawn_blocking(move || {
                            if let Err(e) = store.record_seen(&peer, &addr) {
                                eprintln!("lan event=record_seen_error error={e}");
                            }
                            known_addrs(&store, &peer)
                        })
                        .await
                        .unwrap_or_default();
                        book_tx.send_if_modified(|b| {
                            let changed = b.known != known;
                            b.known = known;
                            changed
                        });
                    }
                    Some(UseEvent::DialFailed { .. }) | Some(UseEvent::Disconnected) => discovery.rebrowse(),
                    None => return,
                },
            }
        }
    });
    Ok((
        UseDiscovery {
            addrs: book_rx,
            events: ev_tx,
        },
        UseLan { task },
    ))
}

/// One sharing Mac seen on the LAN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    pub fp: String,
    pub addrs: Vec<IpAddr>,
    pub port: u16,
    pub pairing_open: bool,
    /// The paired Mac whose PeerId has this fingerprint (claim only — unverified until a
    /// pinned dial succeeds).
    pub paired: Option<TrustedPeer>,
}

/// Browse for `within` and return every sharing Mac seen (latest state per fingerprint).
/// With `stop_when`, returns as soon as an entry satisfies it.
pub async fn discover(
    store: Option<&TrustStore>,
    within: Duration,
    stop_when: Option<&(dyn Fn(&Discovered) -> bool + Sync)>,
) -> Result<Vec<Discovered>, String> {
    let mut d = mdns::browse_continuous().map_err(|e| e.to_string())?;
    let paired: Vec<(String, TrustedPeer)> = store
        .map(|s| s.list())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|p| p.peer().map(|id| (fingerprint(&id), p)))
        .collect();
    let mut seen: BTreeMap<String, Discovered> = BTreeMap::new();
    let deadline = Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        // Looking for something specific: re-query every second (a fresh answer carries
        // the current TXT, e.g. `pair=1` that was set a moment ago).
        let step = if stop_when.is_some() { left.min(Duration::from_secs(1)) } else { left };
        match tokio::time::timeout(step, d.recv()).await {
            Ok(Some(DiscoveryEvent::Found { fp, addrs, port, pairing_open })) => {
                let entry = Discovered {
                    paired: paired.iter().find(|(f, _)| *f == fp).map(|(_, p)| p.clone()),
                    fp: fp.clone(),
                    addrs,
                    port,
                    pairing_open,
                };
                let hit = stop_when.is_some_and(|f| f(&entry));
                seen.insert(fp, entry);
                if hit {
                    break;
                }
            }
            Ok(Some(DiscoveryEvent::Lost { fp })) => {
                seen.remove(&fp);
            }
            Ok(None) => break,
            Err(_) => {
                if stop_when.is_some() {
                    d.rebrowse();
                }
            }
        }
    }
    Ok(seen.into_values().collect())
}

/// Is `s` a fingerprint or a fingerprint prefix (≥ 4 lowercase hex chars)?
pub fn looks_like_fp(s: &str) -> bool {
    (4..=16).contains(&s.len()) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `pair --peer <fp>`: find the Mac showing a pairing code whose fingerprint starts with
/// `fp`, and return its dial candidates (no `/p2p`: the code authenticates it; the
/// connected PeerId is then checked against the fingerprint before pairing).
pub async fn resolve_fp(fp: &str, within: Duration) -> Result<(String, Vec<Multiaddr>), String> {
    let pred = |d: &Discovered| d.fp.starts_with(fp) && d.pairing_open;
    let mut found = discover(None, within, Some(&pred)).await?;
    // A Found can arrive before all of a host's address records; listen a moment more
    // so the dial can race every address.
    if found.iter().any(&pred) {
        let more = discover(None, ADDR_SETTLE, None).await.unwrap_or_default();
        for m in more {
            if let Some(f) = found.iter_mut().find(|f| f.fp == m.fp) {
                let mut all: std::collections::BTreeSet<IpAddr> = f.addrs.iter().copied().collect();
                all.extend(m.addrs);
                f.addrs = all.into_iter().collect();
            }
        }
    }
    let hits: Vec<&Discovered> = found.iter().filter(|d| d.fp.starts_with(fp)).collect();
    let open: Vec<&&Discovered> = hits.iter().filter(|d| d.pairing_open).collect();
    match (open.as_slice(), hits.as_slice()) {
        ([one], _) => {
            let addrs = one
                .addrs
                .iter()
                .map(|ip| {
                    Multiaddr::empty()
                        .with(match ip {
                            IpAddr::V4(v) => Protocol::Ip4(*v),
                            IpAddr::V6(v) => Protocol::Ip6(*v),
                        })
                        .with(Protocol::Udp(one.port))
                        .with(Protocol::QuicV1)
                })
                .collect();
            Ok((one.fp.clone(), addrs))
        }
        ([], []) => Err(format!(
            "no Mac with fingerprint {fp} found on this network within {} s (is it showing a code? Not listed? Use its address: --peer /ip4/<ip>/udp/9003/quic-v1)",
            within.as_secs()
        )),
        ([], _) => Err(format!("the Mac {fp} is on the network but not showing a pairing code; run `slipstream-node pair --show-code` there")),
        (_, _) => Err(format!("several Macs match {fp}; type more of the fingerprint")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_16_hex_of_sha256_peer_bytes() {
        let node = p2p_crypto::NodeKeypair::generate();
        let peer = p2p_forward::transport_identity(&node).public().to_peer_id();
        let fp = fingerprint(&peer);
        assert!(mdns::valid_fp(&fp), "{fp}");
        assert_eq!(fp, hex::encode(&Sha256::digest(peer.to_bytes())[..8]));
        assert_ne!(fp, fingerprint(&PeerId::random()));
    }

    #[test]
    fn pinned_addrs_carry_the_pinned_id_and_announce_rules() {
        let peer = PeerId::random();
        let a = pinned_addrs(peer, &["::1".parse().unwrap(), "10.0.0.5".parse().unwrap()], 9003);
        assert_eq!(a[0].to_string(), format!("/ip4/10.0.0.5/udp/9003/quic-v1/p2p/{peer}"));
        assert_eq!(a[1].to_string(), format!("/ip6/::1/udp/9003/quic-v1/p2p/{peer}"));
        assert!(should_announce(&"/ip4/0.0.0.0/udp/9003/quic-v1".parse().unwrap()));
        assert!(should_announce(&"/ip4/192.168.1.4/udp/9003/quic-v1".parse().unwrap()));
        assert!(!should_announce(&"/ip4/127.0.0.1/udp/9003/quic-v1".parse().unwrap()));
        assert!(!should_announce(&"/ip6/::1/udp/9003/quic-v1".parse().unwrap()));
        assert!(looks_like_fp("ab12") && looks_like_fp("0123456789abcdef"));
        assert!(!looks_like_fp("ab1") && !looks_like_fp("/ip4/1.2.3.4") && !looks_like_fp("AB12"));
    }

    #[test]
    fn candidates_never_take_another_pinned_id() {
        let peer = PeerId::random();
        let other = PeerId::random();
        let addrs: Vec<Multiaddr> = vec![
            format!("/ip4/10.0.0.1/udp/1/quic-v1/p2p/{other}").parse().unwrap(),
            "/ip4/10.0.0.2/udp/2/quic-v1".parse().unwrap(),
            format!("/p2p/{peer}").parse().unwrap(),
        ];
        let c = pinned_candidates(peer, addrs.iter());
        assert_eq!(c, vec![format!("/ip4/10.0.0.2/udp/2/quic-v1/p2p/{peer}").parse::<Multiaddr>().unwrap()]);
    }
}
