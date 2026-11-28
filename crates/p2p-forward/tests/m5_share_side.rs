//! v0.6 M5 share-side checks through the real forwarder (mock llama-server):
//!
//! - busy accounting: the share side counts its own forwarded requests that hold an
//!   upstream connection; "busy with its own work" (`Status::local_busy`) = llama-server
//!   requests (`/metrics` processing + deferred) beyond that count
//! - `POST /completion` (the app's benchmark endpoint) is forwarded as an inference request
//! - the use side keeps its port and answers 503 while the share side is unreachable

mod support;

use std::time::{Duration, Instant};

use p2p_forward::status::{local_requests, StatusReply};
use support::{chat_body, request, rig, send, RigOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn status(r: &support::Rig) -> p2p_forward::Status {
    match r.use_side.status().await.unwrap() {
        StatusReply::Status(s) => s,
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn busy_accounting_counts_forwarded_requests() {
    let r = rig(RigOptions::default()).await;
    r.mock.set_local_requests(0);
    let fwd = r.use_side.bind;
    let host = fwd.to_string();
    let s = status(&r).await;
    assert!(!s.busy && !s.local_busy, "{s:?}");
    assert_eq!(r.share.forwarded(), 0);

    // a slow forwarded stream: llama-server has 1 request, the share side forwarded 1
    let mut sock = TcpStream::connect(fwd).await.unwrap();
    sock.write_all(&request("POST", "/v1/chat/completions?n=400&gap_us=10000", &host, &[], &chat_body("plain")))
        .await
        .unwrap();
    let mut buf = vec![0u8; 8192];
    let n = sock.read(&mut buf).await.unwrap();
    assert!(n > 0);
    let t0 = Instant::now();
    while r.mock.active.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        assert!(t0.elapsed() < Duration::from_secs(3));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(r.share.forwarded(), 1);
    assert_eq!(local_requests(r.mock.addr, r.share.forwarded()).await, Some(0));
    let s = status(&r).await;
    assert!(s.busy && !s.local_busy, "a paired request alone is not 'its own work': {s:?}");

    // the big Mac's own editor sends a request too (it waits in llama-server's queue)
    r.mock.set_local_requests(1);
    assert_eq!(local_requests(r.mock.addr, r.share.forwarded()).await, Some(1));
    let s = status(&r).await;
    assert!(s.local_busy, "{s:?}");
    r.mock.set_local_requests(0);

    // the stream ends (editor closes) → the forwarded count drops back to 0
    drop(sock);
    let t0 = Instant::now();
    while r.share.forwarded() != 0 {
        assert!(t0.elapsed() < Duration::from_secs(3), "forwarded count stuck");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // only local work left → local_busy without any forwarded request
    r.mock.set_local_requests(1);
    let s = status(&r).await;
    assert!(!s.busy && s.local_busy, "{s:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_completion_endpoint_is_forwarded_for_benchmarks() {
    let r = rig(RigOptions::default()).await;
    let fwd = r.use_side.bind;
    let host = fwd.to_string();
    let body = br#"{"prompt":"Benchmark run 1","n_predict":8,"temperature":0,"cache_prompt":false,"stream":false}"#;
    let resp = send(fwd, &request("POST", "/completion", &host, &[("Content-Type", "application/json")], body)).await;
    assert_eq!(resp.status(), 200, "{}", resp.head());
    assert!(resp.body_text().contains("\"timings\""), "{}", resp.body_text());
    let seen = r.mock.requests();
    assert!(seen.iter().any(|s| s.method == "POST" && s.target == "/completion"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unreachable_share_gets_a_503_not_a_dead_port() {
    let r = rig(RigOptions {
        rtt: Some(Duration::from_millis(2)),
        ..RigOptions::default()
    })
    .await;
    let fwd = r.use_side.bind;
    let host = fwd.to_string();
    r.shaper.as_ref().unwrap().control.set_blackhole(true);
    let t0 = Instant::now();
    while r.use_side.connected() {
        assert!(t0.elapsed() < Duration::from_secs(12), "peer loss not detected");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    r.use_side.set_offline(p2p_forward::client::LocalAnswer::new(503, "peer_asleep", "Studio is asleep."));
    let body = chat_body("plain");
    let resp = send(fwd, &request("POST", "/v1/chat/completions", &host, &[], &body)).await;
    assert_eq!(resp.status(), 503, "{}", resp.head());
    assert!(resp.body_text().contains("peer_asleep") && resp.body_text().contains("Studio is asleep."));
    assert!(resp.head().contains("Retry-After: 5"));
    // blocked answers (version mismatch) win even while connected
    r.shaper.as_ref().unwrap().control.set_blackhole(false);
    assert!(r.use_side.wait_connected(Duration::from_secs(15)).await, "no reconnect");
    r.use_side.set_blocked(Some(p2p_forward::client::LocalAnswer::new(426, "version_mismatch", "Update it.")));
    let resp = send(fwd, &request("GET", "/health", &host, &[], b"")).await;
    assert_eq!(resp.status(), 426, "{}", resp.head());
    r.use_side.set_blocked(None);
    let resp = send(fwd, &request("GET", "/health", &host, &[], b"")).await;
    assert_eq!(resp.status(), 200, "{}", resp.head());
}
