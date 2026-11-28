//! v0.6 M2 pairing (plan §2, §7 `pairing.rs`, §9 M2): 6-digit code + SPAKE2 over
//! `/slipstream/pair/1` between separate "Macs" (own data dirs / keys) on loopback QUIC.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{mac, pair, show_code, type_code, wait_until, wrong, LOOPBACK};
use futures::StreamExt;
use p2p_crypto::{PairInfo, PairingCode, PakeRole, PakeStart, Transcript};
use p2p_forward::StaticTrust;
use p2p_libp2p::paired::{PairedConfig, PAIR_PROTOCOL_V1};
use p2p_node::pairing::{connect, read_msg, write_msg, ManualClock, PairError, PairMsg, SystemClock, WindowState};
use p2p_node::trust::PeerRole;
use tokio_util::compat::FuturesAsyncReadCompatExt;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn correct_code_pairs_both_macs_once() {
    let root = tempfile::tempdir().unwrap();
    let big = mac(root.path(), "Mac Studio");
    let book = mac(root.path(), "MacBook");
    let listener = show_code(&big, Arc::new(SystemClock)).await;
    let code = listener.window.code().as_str().to_string();
    let entry = type_code(&book, &listener, &code).await.unwrap();

    // client side: the big Mac, pinned with its static key and address
    assert_eq!(entry.peer_id, big.peer.to_string());
    assert_eq!(entry.encryption_node_id, big.node.public_hex());
    assert_eq!(entry.name, "Mac Studio");
    assert_eq!(entry.role, PeerRole::Share);
    assert!(entry.may_start_model);
    assert!(entry.last_addrs[0].ends_with(&format!("/p2p/{}", big.peer)));
    assert_eq!(entry.identity_hex.len(), 64);
    assert_eq!(book.store.trusted_key(&big.peer), Some(big.node.public_bytes()));
    // server side: the MacBook
    let theirs = big.store.get(&book.peer).expect("big Mac stored the MacBook");
    assert_eq!(theirs.encryption_node_id, book.node.public_hex());
    assert_eq!(theirs.name, "MacBook");
    assert_eq!(theirs.role, PeerRole::Use);
    assert_eq!(theirs.app_version, env!("CARGO_PKG_VERSION"));
    // both persisted
    let reread = p2p_node::trust::TrustStore::load(&big.dir, &big.node).unwrap();
    assert!(reread.is_trusted(&book.peer));

    // the window closed after one success: the same code does not work again
    assert_eq!(listener.window.state(), WindowState::Paired);
    let third = mac(root.path(), "Stranger");
    let err = type_code(&third, &listener, &code).await.unwrap_err();
    assert!(
        matches!(&err, PairError::Refused { .. } | PairError::Unreachable(_)),
        "{err:?}"
    );
    assert!(!big.store.is_trusted(&third.peer));
    assert!(third.store.list().is_empty());
}

/// Pairing duration on loopback (recorded in bench/RESULTS.md "v0.6 M2").
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pairing_duration_on_loopback() {
    let root = tempfile::tempdir().unwrap();
    let mut ms = Vec::new();
    for i in 0..10 {
        let big = mac(root.path(), &format!("big{i}"));
        let book = mac(root.path(), &format!("book{i}"));
        ms.push(pair(&big, &book).await.as_secs_f64() * 1e3);
    }
    let mut sorted = ms.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "pairing_ms n={} min={:.2} p50={:.2} max={:.2} all={:?}",
        ms.len(),
        sorted[0],
        sorted[ms.len() / 2],
        sorted[ms.len() - 1],
        ms.iter().map(|v| format!("{v:.2}")).collect::<Vec<_>>()
    );
    assert!(sorted[ms.len() / 2] < 1000.0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wrong_code_three_times_closes_the_window() {
    let root = tempfile::tempdir().unwrap();
    let big = mac(root.path(), "Mac Studio");
    let book = mac(root.path(), "MacBook");
    let listener = show_code(&big, Arc::new(SystemClock)).await;
    let code = listener.window.code().as_str().to_string();
    for i in 1..=3 {
        let err = type_code(&book, &listener, &wrong(&code)).await.unwrap_err();
        assert!(matches!(err, PairError::WrongCode), "attempt {i}: {err:?}");
        wait_until("failure counted", Duration::from_secs(5), || listener.window.failed_attempts() == i).await;
    }
    assert_eq!(listener.window.state(), WindowState::TooManyFailures);
    // the right code no longer helps: a new code is needed
    let err = type_code(&book, &listener, &code).await.unwrap_err();
    assert!(matches!(&err, PairError::Refused { .. } | PairError::Unreachable(_)), "{err:?}");
    assert!(big.store.list().is_empty());
    assert!(book.store.list().is_empty());
}

/// A relay M terminates both QUIC connections under its own PeerId and pipes the pair
/// stream bytes between A and B (it does not even need to know the code). A computes
/// SPAKE2 with (A, M), B with (M, B) → different keys → key confirmation fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mitm_relay_with_a_different_peer_id_fails_key_confirmation() {
    let root = tempfile::tempdir().unwrap();
    let big = mac(root.path(), "Mac Studio");
    let book = mac(root.path(), "MacBook");
    let mitm = mac(root.path(), "Relay");
    let listener = show_code(&big, Arc::new(SystemClock)).await;
    let code = listener.window.code().as_str().to_string();

    // M listens (accepting anyone for pairing) and relays to B.
    let open_trust = StaticTrust::new([]);
    open_trust.set_pairing_open(true);
    let gated = p2p_forward::gate::start_gated_listener(
        p2p_forward::transport_identity(&mitm.node),
        LOOPBACK.parse().unwrap(),
        &PairedConfig::default(),
        open_trust,
        Default::default(),
        "mitm",
        false,
    )
    .await
    .unwrap();
    let mut incoming = gated.control.clone().accept(PAIR_PROTOCOL_V1).unwrap();
    let to_big = connect(&mitm.node, &listener.dial_addr(), &PairedConfig::default())
        .await
        .unwrap();
    assert_eq!(to_big.remote, big.peer);
    let mut big_control = to_big.control.clone();
    let relay = tokio::spawn(async move {
        let (_peer, from_a) = incoming.next().await.unwrap();
        let to_b = big_control.open_stream(big.peer, PAIR_PROTOCOL_V1).await.unwrap();
        let (mut a, mut b) = (from_a.compat(), to_b.compat());
        let _ = tokio::io::copy_bidirectional(&mut a, &mut b).await;
        big
    });
    let mitm_addr = gated.listen_addr.clone().with(libp2p::multiaddr::Protocol::P2p(gated.peer_id));
    let err = p2p_node::pairing::pair_with(
        &book.node,
        &book.store,
        &mitm_addr,
        &PairingCode::parse(&code).unwrap(),
        p2p_node::pairing::local_info(Some("MacBook".into()), false),
        &PairedConfig::default(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, PairError::WrongCode), "{err:?}");
    assert!(book.store.list().is_empty(), "client stored the relay");
    wait_until("B counted the failed session", Duration::from_secs(5), || {
        listener.window.failed_attempts() == 1
    })
    .await;
    let big = tokio::time::timeout(Duration::from_secs(5), relay).await.unwrap().unwrap();
    assert!(big.store.list().is_empty(), "server stored someone");
    // the real MacBook can still pair (window open, 2 attempts left)
    assert_eq!(listener.window.state(), WindowState::Open);
    type_code(&book, &listener, &code).await.unwrap();
    drop(to_big);
}

/// Scripted client: Start, read Reply, compute our Finish (correct code). Returns the
/// Finish we would send, the stream (still open) and B's confirmation tag.
async fn scripted_session(
    book: &common::Mac,
    listener: &p2p_node::pairing::PairListener,
    code: &str,
) -> (PairMsg, tokio_util::compat::Compat<libp2p::Stream>, p2p_node::pairing::PairConnection) {
    let conn = connect(&book.node, &listener.dial_addr(), &PairedConfig::default())
        .await
        .unwrap();
    let mut control = conn.control.clone();
    let stream = control.open_stream(conn.remote, PAIR_PROTOCOL_V1).await.unwrap();
    let mut io = stream.compat();
    let (id_a, id_b) = (conn.local.to_bytes(), conn.remote.to_bytes());
    let (start, spake_a) = PakeStart::new(PakeRole::Client, &PairingCode::parse(code).unwrap(), &id_a, &id_b);
    write_msg(
        &mut io,
        &PairMsg::Start {
            spake: hex::encode(&spake_a),
            x25519: book.node.public_hex(),
        },
    )
    .await
    .unwrap();
    let PairMsg::Reply { spake, x25519, info, confirm } = read_msg(&mut io).await.unwrap() else {
        panic!("expected Reply");
    };
    let key = start.finish(&hex::decode(spake).unwrap()).unwrap();
    let t = Transcript {
        peer_id_a: &id_a,
        peer_id_b: &id_b,
        x25519_a: book.node.public_bytes(),
        x25519_b: hex::decode(x25519).unwrap().try_into().unwrap(),
    };
    let info_b = key.open_info(&hex::decode(info).unwrap()).unwrap();
    let confirm_b = hex::decode(confirm).unwrap();
    key.verify_peer(&t, &info_b, &[], &confirm_b).unwrap();
    let mine = PairInfo {
        name: "MacBook".into(),
        app_version: "0.6.0".into(),
        may_start_model: false,
    };
    let finish = PairMsg::Finish {
        info: hex::encode(key.seal_info(&mine)),
        confirm: hex::encode(key.confirm(&t, &mine, &confirm_b)),
    };
    (finish, io, conn)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn replayed_confirmation_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let big = mac(root.path(), "Mac Studio");
    let book = mac(root.path(), "MacBook");
    let listener = show_code(&big, Arc::new(SystemClock)).await;
    let code = listener.window.code().as_str().to_string();

    // Session 1: a valid Finish is recorded (by an eavesdropper / the client) but not sent.
    let (recorded, io1, conn1) = scripted_session(&book, &listener, &code).await;
    drop(io1);
    drop(conn1);
    wait_until("session 1 counted", Duration::from_secs(5), || listener.window.failed_attempts() == 1).await;

    // Session 2: same peer, same code, replay session 1's Finish → refused.
    let (_fresh, mut io2, conn2) = scripted_session(&book, &listener, &code).await;
    write_msg(&mut io2, &recorded).await.unwrap();
    match read_msg(&mut io2).await.unwrap() {
        PairMsg::Refused { code, .. } => assert_eq!(code, "wrong_code"),
        other => panic!("replay accepted: {other:?}"),
    }
    drop(io2);
    drop(conn2);
    assert!(big.store.list().is_empty(), "replayed confirmation stored a peer");
    wait_until("session 2 counted", Duration::from_secs(5), || listener.window.failed_attempts() == 2).await;

    // Session 3: a fresh, honest Finish with the same code works → the replay, not the
    // code, was the problem.
    let (fresh, mut io3, _conn3) = scripted_session(&book, &listener, &code).await;
    write_msg(&mut io3, &fresh).await.unwrap();
    assert!(matches!(read_msg(&mut io3).await.unwrap(), PairMsg::Done { .. }));
    assert!(big.store.is_trusted(&book.peer));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn code_expires_after_five_minutes() {
    let root = tempfile::tempdir().unwrap();
    let big = mac(root.path(), "Mac Studio");
    let book = mac(root.path(), "MacBook");
    let clock = Arc::new(ManualClock::default());
    let listener = show_code(&big, clock.clone()).await;
    let code = listener.window.code().as_str().to_string();
    clock.advance(Duration::from_secs(4 * 60 + 59));
    assert_eq!(listener.window.state(), WindowState::Open);
    assert!(p2p_forward::PeerTrust::pairing_open(&*big.trust));
    clock.advance(Duration::from_secs(2));
    assert_eq!(listener.window.state(), WindowState::Expired);
    assert!(!p2p_forward::PeerTrust::pairing_open(&*big.trust), "gate must stop admitting guests");
    let t0 = Instant::now();
    let err = type_code(&book, &listener, &code).await.unwrap_err();
    assert!(matches!(&err, PairError::Refused { .. } | PairError::Unreachable(_)), "{err:?}");
    if let PairError::Refused { code, .. } = &err {
        // connection_closed: the gate dropped the guest connection first (seen once under load).
        assert!(code == "expired" || code == "not_pairing" || code == "connection_closed", "{code}");
    }
    assert!(t0.elapsed() < Duration::from_secs(6));
    assert!(big.store.list().is_empty());
    assert!(book.store.list().is_empty());
}
