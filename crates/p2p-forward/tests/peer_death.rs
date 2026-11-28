//! M1 acceptance "peer death ≤ 10 s" (plan §9): the share side vanishes mid-request
//! (UDP blackhole = big Mac asleep / off the network). With the production transport
//! config (QUIC keep-alive 2 s, idle timeout 8 s) the use side must detect it within
//! 10 s, end the editor's stream with an SSE error event, answer new requests with 502,
//! and the share side must drop its llama-server connection (cancel).

mod support;

use std::time::{Duration, Instant};

use support::{chat_body, request, rig, send, RigOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn peer_death_detected_within_10s_mid_stream() {
    let r = rig(RigOptions {
        rtt: Some(Duration::from_millis(2)),
        ..RigOptions::default()
    })
    .await;
    let fwd = r.use_side.bind;
    let host = fwd.to_string();
    let mut sock = TcpStream::connect(fwd).await.unwrap();
    sock.write_all(&request(
        "POST",
        "/v1/chat/completions?n=100000&gap_us=50000",
        &host,
        &[],
        &chat_body("plain"),
    ))
    .await
    .unwrap();
    let mut buf = vec![0u8; 16384];
    let mut got = Vec::new();
    while got.windows(6).filter(|w| w == b"data: ").count() < 3 {
        let n = sock.read(&mut buf).await.unwrap();
        got.extend_from_slice(&buf[..n]);
    }
    let t0 = Instant::now();
    r.shaper.as_ref().unwrap().control.set_blackhole(true);
    // Read until the forwarder ends the stream.
    let mut tail = Vec::new();
    let res = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match sock.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => tail.extend_from_slice(&buf[..n]),
            }
        }
    })
    .await;
    let detected = t0.elapsed();
    assert!(res.is_ok(), "stream never ended");
    eprintln!("peer_death detected_s={:.2}", detected.as_secs_f64());
    assert!(detected <= Duration::from_secs(10), "detected after {detected:?}");
    let text = String::from_utf8_lossy(&tail);
    assert!(text.contains("peer_lost"), "no SSE error event: …{}", &text[text.len().saturating_sub(300)..]);
    // the swarm's ConnectionClosed event can land just after the stream error
    let deadline = Instant::now() + Duration::from_secs(1);
    while r.use_side.connected() {
        assert!(Instant::now() < deadline, "use side still reports a connection");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(r.use_side.stats.lock().unwrap().peer_lost_mid_request, 1);
    // new requests fail fast with 502 while the peer is gone
    let t = Instant::now();
    let resp = send(fwd, &request("POST", "/v1/chat/completions", &host, &[], &chat_body("plain"))).await;
    assert_eq!(resp.status(), 502, "{}", resp.head());
    assert!(t.elapsed() < Duration::from_secs(3));
    // the share side noticed too and dropped the upstream connection
    let d = r.mock.wait_disconnects(1, Duration::from_secs(5)).await;
    assert_eq!(d.len(), 1, "share side kept llama-server busy after peer loss");
    eprintln!("peer_death share_upstream_dropped_s={:.2}", d[0].at.duration_since(t0).as_secs_f64());
    assert!(d[0].at.duration_since(t0) <= Duration::from_secs(11));
    // peer comes back → the use side re-dials and serves again
    r.shaper.as_ref().unwrap().control.set_blackhole(false);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let resp = send(fwd, &request("GET", "/health", &host, &[], b"")).await;
        if resp.status() == 200 {
            break;
        }
        assert!(Instant::now() < deadline, "no reconnect after the peer came back");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    eprintln!("peer_death reconnected_s={:.2}", t0.elapsed().as_secs_f64());
}
