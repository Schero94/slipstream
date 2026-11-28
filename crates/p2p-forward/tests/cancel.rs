//! M1 cancel propagation (plan §7 `cancel.rs`, §9 acceptance "cancel < 250 ms p95 @5 ms
//! RTT"): the editor closes its connection during prefill (before the first token) or
//! during decode; the share side must drop its llama-server connection quickly. Paths
//! go through the userspace UDP shaper at 5 ms RTT. Latency = editor close → the mock
//! llama-server observing its client connection close.

mod support;

use std::time::{Duration, Instant};

use support::{chat_body, p95, request, rig, RigOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const N: usize = 20;

async fn cancel_latencies(phase: &str) -> Vec<f64> {
    let r = rig(RigOptions {
        rtt: Some(Duration::from_millis(5)),
        ..RigOptions::default()
    })
    .await;
    let fwd = r.use_side.bind;
    let host = fwd.to_string();
    let target = match phase {
        // "prefill": the first token would only come after 30 s
        "prefill" => "/v1/chat/completions?n=4&prefill_ms=30000",
        // "decode": a token every 20 ms for minutes
        _ => "/v1/chat/completions?n=100000&gap_us=20000",
    };
    let mut lat = Vec::new();
    for i in 0..N {
        let mut sock = TcpStream::connect(fwd).await.unwrap();
        sock.set_nodelay(true).unwrap();
        sock.write_all(&request("POST", target, &host, &[], &chat_body("plain")))
            .await
            .unwrap();
        if phase == "prefill" {
            // wait until the request is in llama-server (prefill running)
            let deadline = Instant::now() + Duration::from_secs(5);
            while r.mock.requests().len() <= i {
                assert!(Instant::now() < deadline, "request never reached upstream");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        } else {
            // read until a few tokens arrived
            let mut buf = vec![0u8; 16384];
            let mut events = 0;
            while events < 5 {
                let n = sock.read(&mut buf).await.unwrap();
                assert!(n > 0);
                events += buf[..n].windows(6).filter(|w| w == b"data: ").count();
            }
        }
        let before = r.mock.disconnects().len();
        let t0 = Instant::now();
        drop(sock); // the editor's stop button
        let d = r.mock.wait_disconnects(before + 1, Duration::from_secs(5)).await;
        assert!(d.len() > before, "{phase}: upstream never saw the cancel");
        let last = d.last().unwrap();
        assert_eq!(last.phase, phase);
        lat.push(last.at.duration_since(t0).as_secs_f64() * 1e3);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let ping: Vec<u64> = r.use_side.stats.lock().unwrap().ping_rtt_us.clone();
    eprintln!(
        "cancel phase={phase} n={} p50_ms={:.2} p95_ms={:.2} max_ms={:.2} shaped_ping_rtt_us={:?}",
        lat.len(),
        { let mut v = lat.clone(); v.sort_by(|a, b| a.partial_cmp(b).unwrap()); v[v.len() / 2] },
        p95(&mut lat.clone()),
        lat.iter().cloned().fold(0.0, f64::max),
        &ping[..ping.len().min(5)]
    );
    let s = r.share.stats.lock().unwrap().clone();
    assert_eq!(s.cancelled as usize, N, "{s:?}");
    assert_eq!(r.use_side.stats.lock().unwrap().cancels_sent as usize, N);
    lat
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_during_prefill_under_250ms_p95_at_5ms_rtt() {
    let mut lat = cancel_latencies("prefill").await;
    assert!(p95(&mut lat) < 250.0, "p95 {:.1} ms", p95(&mut lat));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_during_decode_under_250ms_p95_at_5ms_rtt() {
    let mut lat = cancel_latencies("decode").await;
    assert!(p95(&mut lat) < 250.0, "p95 {:.1} ms", p95(&mut lat));
}

/// A queued request (admission) whose editor goes away frees its queue place and never
/// reaches llama-server.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_while_queued_never_reaches_upstream() {
    let r = rig(RigOptions::default()).await;
    let fwd = r.use_side.bind;
    let host = fwd.to_string();
    let mut running = TcpStream::connect(fwd).await.unwrap();
    running
        .write_all(&request("POST", "/v1/chat/completions?n=2&prefill_ms=1500", &host, &[], &chat_body("plain")))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut queued = TcpStream::connect(fwd).await.unwrap();
    queued
        .write_all(&request("POST", "/v1/chat/completions?n=2", &host, &[], &chat_body("plain")))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(r.share.admission.load(), (1, 1));
    drop(queued);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(r.share.admission.load(), (1, 0));
    let mut out = Vec::new();
    running.read_to_end(&mut out).await.unwrap();
    assert!(out.starts_with(b"HTTP/1.1 200"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(r.mock.requests().len(), 1, "the cancelled queued request reached upstream");
}
