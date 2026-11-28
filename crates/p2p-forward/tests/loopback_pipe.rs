//! M0: raw and sealed byte pipes over a persistent paired QUIC connection on loopback.

use std::net::SocketAddr;
use std::time::Duration;

use p2p_forward::bench::run_request;
use p2p_forward::node::{start_share, start_use, ShareHandle, UseHandle};
use p2p_forward::pipe::PipeMode;
use p2p_forward::{load_or_create_identity, mock};
use p2p_libp2p::paired::PairedConfig;
use sha2::{Digest, Sha256};

async fn spawn_mock() -> SocketAddr {
    let listener = mock::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(mock::serve(listener));
    addr
}

async fn pair(
    upstream: SocketAddr,
    share_mode: PipeMode,
    use_mode: PipeMode,
    config: &PairedConfig,
) -> (tempfile::TempDir, ShareHandle, UseHandle) {
    let dir = tempfile::tempdir().unwrap();
    let share = start_share(
        load_or_create_identity(&dir.path().join("big-mac")).unwrap(),
        "/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap(),
        upstream,
        share_mode,
        config,
        false,
    )
    .await
    .unwrap();
    let use_side = start_use(
        load_or_create_identity(&dir.path().join("macbook")).unwrap(),
        share.dial_addr(),
        "127.0.0.1:0".parse().unwrap(),
        use_mode,
        config,
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        use_side.peer_id, share.peer_id,
        "use side dialed the pinned share identity"
    );
    (dir, share, use_side)
}

fn request(path: &str) -> Vec<u8> {
    format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .into_bytes()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn raw_and_sealed_pipes_are_byte_identical() {
    let upstream = spawn_mock().await;
    let expected = hex::encode(Sha256::digest(mock::expected_body(512)));
    for mode in [PipeMode::Raw, PipeMode::Sealed] {
        let (_dir, _share, use_side) = pair(upstream, mode, mode, &PairedConfig::default()).await;
        for _ in 0..5 {
            let direct = run_request(upstream, &request("/mock?n=512"))
                .await
                .unwrap();
            let piped = run_request(use_side.bind, &request("/mock?n=512"))
                .await
                .unwrap();
            assert_eq!(direct.body_sha256, expected);
            assert_eq!(piped.body_sha256, expected, "{mode:?} pipe changed bytes");
            assert_eq!(piped.events, 513);
        }
        // concurrent streams over the one connection
        let mut joins = Vec::new();
        for _ in 0..8 {
            let bind = use_side.bind;
            joins.push(tokio::spawn(async move {
                run_request(bind, &request("/mock?n=200&gap_us=100"))
                    .await
                    .unwrap()
            }));
        }
        let expected200 = hex::encode(Sha256::digest(mock::expected_body(200)));
        for j in joins {
            assert_eq!(j.await.unwrap().body_sha256, expected200);
        }
        let stats = use_side.stats.lock().unwrap();
        assert_eq!(
            stats.connections_established, 1,
            "one persistent connection reused"
        );
        assert_eq!(stats.connections_closed, 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connection_persists_while_idle_and_detects_peer_loss() {
    let upstream = spawn_mock().await;
    let config = PairedConfig {
        keep_alive: Duration::from_millis(200),
        idle_timeout: Duration::from_millis(1000),
        ping_timeout: Duration::from_millis(800),
        ..PairedConfig::default()
    };
    let (_dir, share, use_side) = pair(upstream, PipeMode::Sealed, PipeMode::Sealed, &config).await;
    // idle for > 2x the QUIC idle timeout: keep-alives must hold the connection
    tokio::time::sleep(Duration::from_millis(2500)).await;
    {
        let s = use_side.stats.lock().unwrap();
        assert_eq!(s.connections_closed, 0, "idle connection dropped");
        assert!(s.ping_rtt_us.len() >= 5, "pings every keep_alive interval");
    }
    let r = run_request(use_side.bind, &request("/mock?n=16"))
        .await
        .unwrap();
    assert_eq!(r.events, 17);
    assert_eq!(use_side.stats.lock().unwrap().connections_established, 1);
    // peer disappears → the use side notices within the idle timeout (+ margin)
    drop(share);
    let deadline = tokio::time::Instant::now() + Duration::from_millis(2500);
    loop {
        if use_side.stats.lock().unwrap().connections_closed >= 1 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "peer loss not detected"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sealed_share_never_falls_back_to_plaintext() {
    let upstream = spawn_mock().await;
    // raw client against a sealed share: the request is not valid frames → no upstream
    // response may come back in plaintext.
    let (_dir, _share, use_side) = pair(
        upstream,
        PipeMode::Sealed,
        PipeMode::Raw,
        &PairedConfig::default(),
    )
    .await;
    let r = tokio::time::timeout(
        Duration::from_secs(5),
        run_request(use_side.bind, &request("/mock?n=8")),
    )
    .await
    .expect("pipe must close, not hang");
    if let Ok(sample) = r {
        assert!(!sample.status_line.starts_with("HTTP/1.1 200"));
        assert_eq!(sample.events, 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn use_side_requires_pinned_peer_id() {
    let dir = tempfile::tempdir().unwrap();
    let err = start_use(
        load_or_create_identity(dir.path()).unwrap(),
        "/ip4/127.0.0.1/udp/9/quic-v1".parse().unwrap(),
        "127.0.0.1:0".parse().unwrap(),
        PipeMode::Sealed,
        &PairedConfig::default(),
        false,
    )
    .await
    .err()
    .expect("unpinned address rejected");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn identity_is_persistent_per_key_dir() {
    let dir = tempfile::tempdir().unwrap();
    let a = load_or_create_identity(dir.path())
        .unwrap()
        .public()
        .to_peer_id();
    let b = load_or_create_identity(dir.path())
        .unwrap()
        .public()
        .to_peer_id();
    let other = tempfile::tempdir().unwrap();
    let c = load_or_create_identity(other.path())
        .unwrap()
        .public()
        .to_peer_id();
    assert_eq!(a, b);
    assert_ne!(a, c);
}
