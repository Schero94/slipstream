//! v0.6 M5 edge cases (plan §5) on one Mac, in-process, with the same session code the app
//! and the CLI use (`p2p_node::session`): two data dirs (big Mac / MacBook), a mock
//! llama-server, no mDNS (pairing and use go by address), Wake-on-LAN packets sent to a
//! loopback UDP socket (never the LAN).
//!
//! - asleep: editor port stays bound, 503 `peer_waking` → magic packets → 503 `peer_asleep`
//!   after the waking window → the big Mac comes back → connected, 200
//! - version mismatch: both directions, clear 426 for editors, share side refuses cleanly
//! - removed while offline: the big Mac removes the MacBook while sharing is off; at the
//!   MacBook's next (refused) connection it gets an authenticated Unpair → "removed"

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use p2p_forward::proto::Versions;
use p2p_node::session::{Node, ShareOpts, ShareSession, UseOpts, UseSession, UseState};
use p2p_node::wake::WakeTargets;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn free_tcp() -> SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap()
}

async fn get(addr: SocketAddr, path: &str) -> String {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut out)).await;
    String::from_utf8_lossy(&out).into_owned()
}

async fn wait_state(u: &UseSession, what: &str, secs: u64, f: impl Fn(&UseState) -> bool) -> UseState {
    let t0 = Instant::now();
    loop {
        let st = u.state();
        if f(&st) {
            return st;
        }
        assert!(t0.elapsed() < Duration::from_secs(secs), "timed out waiting for {what}; state {st:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn share_opts(upstream: SocketAddr, listen: &str, versions: Versions) -> ShareOpts {
    let mut o = ShareOpts::new(upstream);
    o.listen = Some(listen.into());
    o.name = Some("Studio".into());
    o.announce = false;
    o.versions = versions;
    o
}

fn use_opts(peer: &str, bind: SocketAddr, wake: WakeTargets) -> UseOpts {
    let mut o = UseOpts::new(peer, bind);
    o.discovery = false;
    o.wake = wake;
    o.wake_grace = Duration::from_millis(300);
    o.wake_window = Duration::from_secs(2);
    o
}

/// Pair book with big through a running share; returns (big share, big PeerId string).
async fn pair(big: &Arc<Node>, book: &Arc<Node>, upstream: SocketAddr, versions: Versions) -> (ShareSession, String) {
    let share = ShareSession::start(big.clone(), share_opts(upstream, "/ip4/127.0.0.1/udp/0/quic-v1", versions))
        .await
        .unwrap();
    let code = share.open_pairing().code;
    let p = p2p_node::session::pair_with_target(book, &share.dial_addr(), &[], &code, Some("MacBook".into()))
        .await
        .unwrap();
    (share, p.peer_id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn asleep_wakes_with_magic_packets_then_says_asleep_then_reconnects() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = p2p_forward::mock_llama::start("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let big = Node::open(&tmp.path().join("big")).unwrap();
    let book = Node::open(&tmp.path().join("book")).unwrap();
    let (share, big_peer) = pair(&big, &book, mock.addr, Versions::default()).await;
    let port = share.port().unwrap();
    // the big Mac's MAC addresses arrived encrypted at pairing and are in the trust store
    let entry = book.store.get(&big_peer.parse().unwrap()).unwrap();
    assert_eq!(entry.wake_macs, p2p_node::wake::local_macs());
    drop(share); // the big Mac falls asleep
    tokio::time::sleep(Duration::from_millis(300)).await;

    let wol = UdpSocket::bind("127.0.0.1:0").unwrap();
    wol.set_nonblocking(true).unwrap();
    let bind = free_tcp();
    let t0 = Instant::now();
    let u = UseSession::start(
        book.clone(),
        use_opts(&big_peer, bind, WakeTargets::Fixed(vec![wol.local_addr().unwrap()])),
    )
    .unwrap();
    // editor port is bound at once and answers 503 while waking (not "connection refused")
    tokio::time::sleep(Duration::from_millis(200)).await;
    let r = get(bind, "/v1/models").await;
    assert!(r.starts_with("HTTP/1.1 503") && r.contains("peer_waking") && r.contains("Retry-After"), "{r}");
    let st = wait_state(&u, "waking", 5, |s| matches!(s, UseState::Waking { .. })).await;
    println!("waking_after_ms={:.0} state={st:?}", t0.elapsed().as_secs_f64() * 1e3);
    let st = wait_state(&u, "asleep", 6, |s| matches!(s, UseState::Asleep { .. })).await;
    let asleep_ms = t0.elapsed().as_secs_f64() * 1e3;
    println!("asleep_after_ms={asleep_ms:.0} state={st:?}");
    assert!(asleep_ms >= 2000.0, "asleep before the waking window ended");
    // magic packets (one per known MAC per resend) went to the fixed target
    let mut packets = Vec::new();
    let mut buf = [0u8; 256];
    while let Ok((n, _)) = wol.recv_from(&mut buf) {
        packets.push(buf[..n].to_vec());
    }
    if entry.wake_macs.is_empty() {
        assert!(packets.is_empty());
    } else {
        let mac = p2p_node::wake::parse_mac(&entry.wake_macs[0]).unwrap();
        assert!(packets.iter().any(|p| p[..] == p2p_node::wake::magic_packet(mac)[..]), "no magic packet");
        println!("magic_packets={}", packets.len());
    }
    let r = get(bind, "/v1/chat/completions").await; // GET → 403 locally (allowlist first)
    assert!(r.starts_with("HTTP/1.1 403"), "{r}");
    let r = get(bind, "/health").await;
    assert!(r.starts_with("HTTP/1.1 503") && r.contains("peer_asleep"), "{r}");

    // the big Mac wakes up (share back on the same port) → connected, requests forwarded
    let back = ShareSession::start(
        big.clone(),
        share_opts(mock.addr, &format!("/ip4/127.0.0.1/udp/{port}/quic-v1"), Versions::default()),
    )
    .await
    .unwrap();
    let t1 = Instant::now();
    wait_state(&u, "connected again", 15, |s| *s == UseState::Connected).await;
    println!("redial_after_wake_ms={:.0}", t1.elapsed().as_secs_f64() * 1e3);
    let r = get(bind, "/v1/models").await;
    assert!(r.starts_with("HTTP/1.1 200"), "{r}");
    drop(u);
    drop(back);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn version_mismatch_is_clear_on_both_sides() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = p2p_forward::mock_llama::start("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let big = Node::open(&tmp.path().join("big")).unwrap();
    let book = Node::open(&tmp.path().join("book")).unwrap();

    // (a) the big Mac is too old for this MacBook: its status says proto 0
    let (share, big_peer) = pair(&big, &book, mock.addr, Versions { proto: 0, min_peer: 0 }).await;
    let bind = free_tcp();
    let u = UseSession::start(book.clone(), use_opts(&big_peer, bind, WakeTargets::Off)).unwrap();
    let st = wait_state(&u, "mismatch (peer)", 10, |s| matches!(s, UseState::VersionMismatch { .. })).await;
    assert!(matches!(&st, UseState::VersionMismatch { side, .. } if side == "peer"), "{st:?}");
    let r = get(bind, "/v1/models").await;
    assert!(r.starts_with("HTTP/1.1 426"), "{r}");
    assert!(r.contains("version_mismatch") && r.contains("Update it to 0.6 or newer"), "{r}");
    let n = mock.requests().len();
    drop(u);
    drop(share);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(mock.requests().len(), n, "nothing forwarded while blocked");

    // (b) this MacBook is too old for the big Mac: the share side refuses cleanly
    let tmp2 = tempfile::tempdir().unwrap();
    let big = Node::open(&tmp2.path().join("big")).unwrap();
    let book = Node::open(&tmp2.path().join("book")).unwrap();
    let (share, big_peer) = pair(&big, &book, mock.addr, Versions { proto: 2, min_peer: 2 }).await;
    let bind = free_tcp();
    let u = UseSession::start(book.clone(), use_opts(&big_peer, bind, WakeTargets::Off)).unwrap();
    let st = wait_state(&u, "mismatch (self)", 10, |s| matches!(s, UseState::VersionMismatch { .. })).await;
    assert!(matches!(&st, UseState::VersionMismatch { side, .. } if side == "self"), "{st:?}");
    let r = get(bind, "/health").await;
    assert!(r.starts_with("HTTP/1.1 426") && r.contains("Update this Mac"), "{r}");
    assert!(share.handle.stats.lock().unwrap().version_refused >= 1);
    // a modified MacBook that skips the check gets the share side's 426 for requests too
    let raw = p2p_forward::proto::ReqHead {
        method: "GET".into(),
        target: "/health".into(),
        headers: vec![],
        body: p2p_forward::proto::BodyKind::None,
        proto: Some(1),
    };
    assert!(p2p_forward::proto::peer_proto(raw.proto) < 2);
    drop(u);
    drop(share);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removed_while_offline_learns_it_at_the_next_connection() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = p2p_forward::mock_llama::start("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let big = Node::open(&tmp.path().join("big")).unwrap();
    let book = Node::open(&tmp.path().join("book")).unwrap();
    let (share, big_peer) = pair(&big, &book, mock.addr, Versions::default()).await;
    let port = share.port().unwrap();
    drop(share); // sharing off
    tokio::time::sleep(Duration::from_millis(300)).await;
    // the big Mac removes the MacBook while nothing is connected (no Unpair possible now)
    let removed = big.store.revoke("MacBook").unwrap();
    let book_peer: p2p_forward::PeerId = removed.peer_id.parse().unwrap();
    assert!(big.store.former_key(&book_peer).is_some(), "tombstone kept");
    assert!(book.store.is_trusted(&big_peer.parse().unwrap()), "MacBook does not know yet");

    let share = ShareSession::start(
        big.clone(),
        share_opts(mock.addr, &format!("/ip4/127.0.0.1/udp/{port}/quic-v1"), Versions::default()),
    )
    .await
    .unwrap();
    let bind = free_tcp();
    let t0 = Instant::now();
    let u = UseSession::start(book.clone(), use_opts(&big_peer, bind, WakeTargets::Off)).unwrap();
    wait_state(&u, "removed", 10, |s| *s == UseState::Removed).await;
    println!("removed_detected_ms={:.0}", t0.elapsed().as_secs_f64() * 1e3);
    assert!(!book.store.is_trusted(&big_peer.parse().unwrap()));
    assert!(share.handle.stats.lock().unwrap().former_unpairs >= 1);
    assert_eq!(share.handle.stats.lock().unwrap().requests, 0, "no forwarder stream for a removed Mac");
    drop(u);
    // port released
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(std::net::TcpListener::bind(bind).is_ok());
    // re-pairing drops the tombstone
    let code = share.open_pairing().code;
    p2p_node::session::pair_with_target(&book, &share.dial_addr(), &[], &code, Some("MacBook".into()))
        .await
        .unwrap();
    assert!(big.store.former_key(&book_peer).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn editor_port_taken_by_another_program_is_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let mock = p2p_forward::mock_llama::start("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let big = Node::open(&tmp.path().join("big")).unwrap();
    let book = Node::open(&tmp.path().join("book")).unwrap();
    let (share, big_peer) = pair(&big, &book, mock.addr, Versions::default()).await;
    let squatter = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let bind = squatter.local_addr().unwrap();
    let u = UseSession::start(book.clone(), use_opts(&big_peer, bind, WakeTargets::Off)).unwrap();
    let st = wait_state(&u, "port busy", 5, |s| matches!(s, UseState::PortBusy { .. })).await;
    println!("port_busy state={st:?}");
    // the other program leaves → the session takes the port (retry every 5 s) and connects
    drop(squatter);
    wait_state(&u, "connected after the port was freed", 15, |s| *s == UseState::Connected).await;
    assert!(get(bind, "/health").await.starts_with("HTTP/1.1 200"));
    drop(u);
    drop(share);
}
