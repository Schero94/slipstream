//! Shared helpers for the M2 pairing / trust-gate tests: "Macs" with their own data dir,
//! node key and trust store, paired over real loopback QUIC.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use libp2p::PeerId;
use p2p_crypto::{NodeKeypair, PairingCode};
use p2p_libp2p::paired::PairedConfig;
use p2p_node::pairing::{local_info, pair_with, start_pair_listener, Clock, PairListener, PairingWindow, SystemClock};
use p2p_node::trust::{NodeTrust, TrustStore, TrustedPeer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub struct Mac {
    pub name: String,
    pub dir: PathBuf,
    pub node: NodeKeypair,
    pub store: Arc<TrustStore>,
    pub trust: Arc<NodeTrust>,
    pub peer: PeerId,
}

pub fn mac(root: &std::path::Path, name: &str) -> Mac {
    let dir = root.join(name.replace(' ', "-"));
    let node = p2p_forward::load_or_create_node(&dir).unwrap();
    let store = TrustStore::load(&dir, &node).unwrap();
    let trust = NodeTrust::new(store.clone(), node.clone());
    let peer = p2p_forward::transport_identity(&node).public().to_peer_id();
    Mac {
        name: name.into(),
        dir,
        node,
        store,
        trust,
        peer,
    }
}

pub const LOOPBACK: &str = "/ip4/127.0.0.1/udp/0/quic-v1";

/// `big` opens a pairing window (its own listener) with `clock`.
pub async fn show_code(big: &Mac, clock: Arc<dyn Clock>) -> PairListener {
    let window = PairingWindow::open(clock, big.store.epoch());
    start_pair_listener(
        big.trust.clone(),
        LOOPBACK.parse().unwrap(),
        &PairedConfig::default(),
        local_info(Some(big.name.clone()), true),
        window,
    )
    .await
    .unwrap()
}

pub async fn type_code(book: &Mac, listener: &PairListener, code: &str) -> Result<TrustedPeer, p2p_node::pairing::PairError> {
    pair_with(
        &book.node,
        &book.store,
        &listener.dial_addr(),
        &PairingCode::parse(code).unwrap(),
        local_info(Some(book.name.clone()), false),
        &PairedConfig::default(),
    )
    .await
}

/// Full pairing; returns the client-side duration (dial + SPAKE2 + confirmation + store).
pub async fn pair(big: &Mac, book: &Mac) -> Duration {
    let listener = show_code(big, Arc::new(SystemClock)).await;
    let code = listener.window.code().as_str().to_string();
    let t0 = Instant::now();
    type_code(book, &listener, &code).await.unwrap();
    let took = t0.elapsed();
    // the server stores before it sends Done, so both sides are paired now
    assert!(big.store.is_trusted(&book.peer));
    assert!(book.store.is_trusted(&big.peer));
    took
}

/// A different code than `code`.
pub fn wrong(code: &str) -> String {
    let n: u32 = code.parse().unwrap();
    format!("{:06}", (n + 1) % 1_000_000)
}

pub async fn wait_until(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

pub fn chat_body() -> Vec<u8> {
    br#"{"model":"slipstream","stream":true,"messages":[{"role":"user","content":"hi"}]}"#.to_vec()
}

/// A raw HTTP/1.1 request; a query in `target` is also sent as `X-Mock-Params`.
pub fn request(method: &str, target: &str, host: &str, body: &[u8]) -> Vec<u8> {
    let mut s = format!("{method} {target} HTTP/1.1\r\nHost: {host}\r\n");
    if let Some((_, q)) = target.split_once('?') {
        s.push_str(&format!("X-Mock-Params: {q}\r\n"));
    }
    if !body.is_empty() || method == "POST" {
        s.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    s.push_str("Connection: close\r\n\r\n");
    let mut v = s.into_bytes();
    v.extend_from_slice(body);
    v
}

pub async fn send(addr: SocketAddr, req: &[u8]) -> String {
    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(req).await.unwrap();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), sock.read_to_end(&mut out)).await;
    String::from_utf8_lossy(&out).into_owned()
}
