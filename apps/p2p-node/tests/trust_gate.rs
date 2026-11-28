//! v0.6 M2 trust gate (plan §2, §7 `trust_gate.rs`, §8): share/use with the real trust
//! store and pairing, separate "Macs" on loopback QUIC, mock llama-server upstream.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{chat_body, mac, pair, request, send, wait_until, Mac, LOOPBACK};
use futures::StreamExt;
use p2p_forward::mock_llama::{self, MockLlama};
use p2p_forward::{start_share, start_use, ShareConfig, ShareHandle, StaticTrust, UseConfig, UseHandle};
use p2p_libp2p::paired::{build_paired_swarm, PairedConfig, OPENAI_PROTOCOL_V1};
use p2p_node::pairing::{attach_pair_protocol, local_info, PairCtx, PairingWindow, SystemClock};
use p2p_node::trust::{resolve_use_target, PeerRole, TrustStore, TrustedPeer, WATCH_INTERVAL};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

struct Running {
    share: ShareHandle,
    _tasks: Vec<tokio::task::JoinHandle<()>>,
}

/// `slipstream-node share` in-process: trust store + watcher + gate + Unpair handler.
async fn share(big: &Mac, mock: &MockLlama) -> Running {
    let watcher = big.store.spawn_watcher(WATCH_INTERVAL);
    let share = start_share(
        p2p_forward::transport_identity(&big.node),
        ShareConfig {
            listen: LOOPBACK.parse().unwrap(),
            upstream: mock.addr,
            trust: big.trust.clone(),
            node: big.node.clone(),
            paired: PairedConfig::default(),
            admission: Default::default(),
            limits: Default::default(),
            name: big.name.clone(),
            model: None,
            versions: Default::default(),
            verbose: false,
        },
    )
    .await
    .unwrap();
    let pair = attach_pair_protocol(share.control(), PairCtx::new(big.trust.clone(), local_info(Some(big.name.clone()), true))).unwrap();
    Running {
        share,
        _tasks: vec![watcher, pair],
    }
}

/// `slipstream-node use` in-process: dial target from the trust store.
async fn use_side(book: &Mac, selector: Option<&str>) -> std::io::Result<(UseHandle, Vec<tokio::task::JoinHandle<()>>)> {
    let (_, peer) = resolve_use_target(&book.store, selector).map_err(std::io::Error::other)?;
    let handle = start_use(
        p2p_forward::transport_identity(&book.node),
        UseConfig {
            peer,
            trust: book.trust.clone(),
            node: book.node.clone(),
            bind: "127.0.0.1:0".parse().unwrap(),
            paired: PairedConfig::default(),
            spare_stream: true,
            token: None,
            local: Default::default(),
            discovery: None,
            app_origin: None,
            persistent: false,
            connect_wait: p2p_forward::client::CONNECT_WAIT,
            versions: Default::default(),
            before_dial: None,
            verbose: false,
        },
    )
    .await?;
    let watcher = book.store.spawn_watcher(WATCH_INTERVAL);
    let pair = attach_pair_protocol(handle.control(), PairCtx::new(book.trust.clone(), local_info(Some(book.name.clone()), false))).unwrap();
    Ok((handle, vec![watcher, pair]))
}

/// Point the pinned entry of `big` in `book`'s store at the running share's address.
fn learn_addr(book: &Mac, big: &Mac, share: &ShareHandle) {
    let mut e = book.store.get(&big.peer).unwrap();
    e.last_addrs = vec![share.dial_addr().to_string()];
    book.store.add(e).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unpaired_peer_disconnected_before_any_stream() {
    let root = tempfile::tempdir().unwrap();
    let big = mac(root.path(), "Mac Studio");
    let book = mac(root.path(), "MacBook");
    pair(&big, &book).await;
    let mock = mock_llama::start("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let run = share(&big, &mock).await;

    // A stranger dials the share side and tries to open a forwarder stream.
    let stranger = mac(root.path(), "Stranger");
    let attempt = |who: &Mac| {
        let mut swarm = build_paired_swarm(p2p_forward::transport_identity(&who.node), &PairedConfig::default());
        swarm.dial(run.share.dial_addr()).unwrap();
        let control = swarm.behaviour().stream.new_control();
        let task = tokio::spawn(async move {
            loop {
                swarm.select_next_some().await;
            }
        });
        (control, task)
    };
    let (mut control, task) = attempt(&stranger);
    let opened = tokio::time::timeout(Duration::from_secs(3), control.open_stream(big.peer, OPENAI_PROTOCOL_V1)).await;
    if let Ok(Ok(stream)) = opened {
        // even if a stream raced the close, nothing is served on it
        let mut io = tokio_util::compat::FuturesAsyncReadCompatExt::compat(stream);
        let mut b = [0u8; 32];
        assert!(io.read_exact(&mut b).await.is_err(), "stranger got a handshake");
    }
    task.abort();
    wait_until("stranger rejected", Duration::from_secs(2), || {
        run.share.stats.lock().unwrap().connections_rejected >= 1
    })
    .await;
    {
        let s = run.share.stats.lock().unwrap();
        assert_eq!(s.streams_accepted, 0, "a stream of an unpaired peer was read");
        assert_eq!(s.connections_established, 0);
    }
    assert!(mock.requests().is_empty());

    // With a pairing window open the stranger may stay connected — but only the pair
    // protocol serves it: the forwarder stream is refused per stream.
    let window = PairingWindow::open(Arc::new(SystemClock), big.store.epoch());
    big.trust.set_window(Some(window.clone()));
    let (mut control, task) = attempt(&stranger);
    let res = tokio::time::timeout(Duration::from_secs(3), control.open_stream(big.peer, OPENAI_PROTOCOL_V1)).await;
    if let Ok(Ok(stream)) = res {
        let mut io = tokio_util::compat::FuturesAsyncReadCompatExt::compat(stream);
        let mut b = [0u8; 32];
        assert!(io.read_exact(&mut b).await.is_err(), "guest got a forwarder handshake");
    }
    wait_until("guest admitted, stream refused", Duration::from_secs(2), || {
        let s = run.share.stats.lock().unwrap();
        s.pairing_guests >= 1 && s.streams_rejected >= 1
    })
    .await;
    assert_eq!(run.share.stats.lock().unwrap().streams_accepted, 0);
    // window closes → the guest is disconnected by the gate
    window.cancel();
    wait_until("guest disconnected after the window closed", Duration::from_secs(1), || {
        run.share.stats.lock().unwrap().guests_closed >= 1
    })
    .await;
    task.abort();
    assert!(mock.requests().is_empty());

    // the paired MacBook works
    learn_addr(&book, &big, &run.share);
    let (handle, _t) = use_side(&book, None).await.unwrap();
    let host = handle.bind.to_string();
    let resp = send(handle.bind, &request("POST", "/v1/chat/completions", &host, &chat_body())).await;
    assert!(resp.contains("data: [DONE]"), "{resp}");
    assert!(run.share.stats.lock().unwrap().streams_accepted >= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoked_peer_mid_stream_is_aborted_within_1s() {
    let root = tempfile::tempdir().unwrap();
    let big = mac(root.path(), "Mac Studio");
    let book = mac(root.path(), "MacBook");
    pair(&big, &book).await;
    let mock = mock_llama::start("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let run = share(&big, &mock).await;
    learn_addr(&book, &big, &run.share);
    let (handle, _t) = use_side(&book, Some("Mac Studio")).await.unwrap();
    let host = handle.bind.to_string();

    let mut sock = TcpStream::connect(handle.bind).await.unwrap();
    sock.write_all(&request(
        "POST",
        "/v1/chat/completions?n=100000&gap_us=20000",
        &host,
        &chat_body(),
    ))
    .await
    .unwrap();
    let mut buf = vec![0u8; 16384];
    let mut got = Vec::new();
    while got.windows(6).filter(|w| w == b"data: ").count() < 5 {
        let n = sock.read(&mut buf).await.unwrap();
        assert!(n > 0);
        got.extend_from_slice(&buf[..n]);
    }
    let before = mock.disconnects().len();

    // `slipstream-node trust revoke "MacBook"` in another process: a second store
    // instance on the same file; the running share notices through its file watcher.
    let cli = TrustStore::load(&big.dir, &big.node).unwrap();
    let t0 = Instant::now();
    cli.revoke("MacBook").unwrap();

    let editor = tokio::spawn(async move {
        let mut buf = vec![0u8; 16384];
        loop {
            match sock.read(&mut buf).await {
                Ok(0) | Err(_) => return Instant::now(),
                Ok(_) => {}
            }
        }
    });
    let d = mock.wait_disconnects(before + 1, Duration::from_secs(3)).await;
    assert!(d.len() > before, "llama-server never saw the abort");
    let upstream_ms = t0.elapsed().as_secs_f64() * 1e3;
    let editor_end = tokio::time::timeout(Duration::from_secs(3), editor).await.unwrap().unwrap();
    let editor_ms = editor_end.duration_since(t0).as_secs_f64() * 1e3;
    println!("revoke_abort upstream_dropped_ms={upstream_ms:.1} editor_stream_ended_ms={editor_ms:.1}");
    assert!(upstream_ms <= 1000.0, "upstream aborted after {upstream_ms:.0} ms");
    assert!(editor_ms <= 1000.0, "editor stream ended after {editor_ms:.0} ms");
    assert!(run.share.stats.lock().unwrap().revoked_streams >= 1);

    // the connection is closed and the best-effort Unpair reached the MacBook
    wait_until("revoked connection closed", Duration::from_secs(2), || {
        run.share.stats.lock().unwrap().revoked_disconnects >= 1
    })
    .await;
    wait_until("Unpair received by the MacBook", Duration::from_secs(2), || !book.store.is_trusted(&big.peer)).await;
    // new requests fail and nothing reaches llama-server
    let n_req = mock.requests().len();
    let resp = send(handle.bind, &request("GET", "/v1/models", &host, b"")).await;
    // M5: no connection → a clear 503 JSON error (was 502 from the failed stream open)
    assert!(resp.starts_with("HTTP/1.1 503") && resp.contains("peer_unreachable"), "{resp}");
    assert_eq!(mock.requests().len(), n_req);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dial_with_wrong_pinned_peer_id_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let big = mac(root.path(), "Mac Studio");
    let book = mac(root.path(), "MacBook");
    pair(&big, &book).await;
    let mock = mock_llama::start("127.0.0.1:0".parse().unwrap()).await.unwrap();

    // An impostor serves at the address the MacBook will dial; it even "trusts" the
    // MacBook. The MacBook pins the big Mac's PeerId → QUIC TLS refuses the impostor.
    let evil = mac(root.path(), "Evil");
    let evil_share = start_share(
        p2p_forward::transport_identity(&evil.node),
        ShareConfig {
            listen: LOOPBACK.parse().unwrap(),
            upstream: mock.addr,
            trust: StaticTrust::new([(book.peer, book.node.public_bytes())]),
            node: evil.node.clone(),
            paired: PairedConfig::default(),
            admission: Default::default(),
            limits: Default::default(),
            name: "Mac Studio".into(),
            model: None,
            versions: Default::default(),
            verbose: false,
        },
    )
    .await
    .unwrap();
    let hijacked = evil_share
        .listen_addr
        .clone()
        .with(libp2p::multiaddr::Protocol::P2p(big.peer));
    let mut e = book.store.get(&big.peer).unwrap();
    e.last_addrs = vec![hijacked.to_string()];
    book.store.add(e.clone()).unwrap();
    let err = use_side(&book, None).await.err().expect("dial to an impostor must fail");
    println!("wrong pinned peer id: {err}");
    assert!(mock.requests().is_empty());
    assert_eq!(evil_share.stats.lock().unwrap().streams_accepted, 0);

    // A stored address that pins another PeerId than the entry is refused before dialing.
    e.last_addrs = vec![evil_share.dial_addr().to_string()];
    book.store.add(e).unwrap();
    let err = resolve_use_target(&book.store, Some("Mac Studio")).unwrap_err();
    assert!(err.contains("refusing to dial a different Mac"), "{err}");
    // An address of an unpaired PeerId is refused before dialing.
    let err = resolve_use_target(&book.store, Some(&evil_share.dial_addr().to_string())).unwrap_err();
    assert!(err.contains("not a paired Mac"), "{err}");
    // And the forwarder itself refuses an untrusted pinned PeerId.
    let res = start_use(
        p2p_forward::transport_identity(&book.node),
        UseConfig {
            peer: evil_share.dial_addr(),
            trust: book.trust.clone(),
            node: book.node.clone(),
            bind: "127.0.0.1:0".parse().unwrap(),
            paired: PairedConfig::default(),
            spare_stream: false,
            token: None,
            local: Default::default(),
            discovery: None,
            app_origin: None,
            persistent: false,
            connect_wait: p2p_forward::client::CONNECT_WAIT,
            versions: Default::default(),
            before_dial: None,
            verbose: false,
        },
    )
    .await;
    let err = res.err().expect("untrusted PeerId must be refused");
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "{err}");
    // No paired Mac at all → clear error
    let empty = mac(root.path(), "Fresh");
    let err = resolve_use_target(&empty.store, None).unwrap_err();
    assert!(err.contains("No paired Mac"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tampered_trust_file_is_empty_with_flag_and_fails_closed() {
    let root = tempfile::tempdir().unwrap();
    let big = mac(root.path(), "Mac Studio");
    let book = mac(root.path(), "MacBook");
    pair(&big, &book).await;
    let mock = mock_llama::start("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let run = share(&big, &mock).await;
    learn_addr(&book, &big, &run.share);
    let (handle, _t) = use_side(&book, None).await.unwrap();
    let host = handle.bind.to_string();
    let ok = send(handle.bind, &request("GET", "/v1/models", &host, b"")).await;
    assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");

    // Someone edits the big Mac's trust file (e.g. adds themselves / renames an entry).
    let path = big.dir.join(p2p_node::trust::TRUST_FILE);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("MacBook"));
    std::fs::write(&path, text.replace("MacBook", "MacB00k")).unwrap();

    // the running share fails closed within ~1 s
    wait_until("running share treats the store as empty", Duration::from_secs(1), || {
        big.store.list().is_empty() && big.store.pairings_reset()
    })
    .await;
    wait_until("paired connection closed", Duration::from_secs(2), || {
        run.share.stats.lock().unwrap().revoked_disconnects >= 1
    })
    .await;
    let n_req = mock.requests().len();
    let resp = send(handle.bind, &request("GET", "/v1/models", &host, b"")).await;
    assert!(!resp.starts_with("HTTP/1.1 200"), "{resp}");
    assert_eq!(mock.requests().len(), n_req);

    // a fresh load sees the same: empty + "pairings were reset"
    let fresh = TrustStore::load(&big.dir, &big.node).unwrap();
    assert!(fresh.list().is_empty());
    assert!(fresh.pairings_reset());
    let err = p2p_node::trust::share_requires_pairing(&fresh).unwrap_err();
    assert!(err.contains("pairings were reset"), "{err}");

    // An entry injected without the MAC key is never trusted.
    let fake = TrustedPeer {
        peer_id: book.peer.to_string(),
        identity_hex: String::new(),
        encryption_node_id: book.node.public_hex(),
        name: "Injected".into(),
        role: PeerRole::Use,
        paired_at: 0,
        last_addrs: vec![],
        last_seen: None,
        app_version: "0".into(),
        may_start_model: true,
        wake_macs: vec![],
    };
    let forged = serde_json::json!({"body": {"version": 1, "peers": [fake]}, "mac": "00".repeat(32)});
    std::fs::write(&path, serde_json::to_vec(&forged).unwrap()).unwrap();
    let s = TrustStore::load(&big.dir, &big.node).unwrap();
    assert!(!s.is_trusted(&book.peer) && s.pairings_reset());
}
