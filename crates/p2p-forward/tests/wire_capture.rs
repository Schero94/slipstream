//! "pcap check" (plan §8; v0.6 security review §8 gap) without tcpdump or sudo: the use side
//! dials the share side through the userspace UDP shaper, which records every QUIC datagram
//! in both directions. A unique plaintext marker in the prompt, the response text, the HTTP
//! request line and the local bearer token must never appear in any captured datagram (QUIC
//! TLS 1.3 outside, SealedStream inside).

mod support;

use std::time::Duration;

use support::{request, rig, send, RigOptions};

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_plaintext_on_the_wire() {
    let token = "wiretoken0123456789abcdefWIRE";
    let r = rig(RigOptions {
        rtt: Some(Duration::from_millis(1)),
        token: Some(token.into()),
        ..RigOptions::default()
    })
    .await;
    let shaper = r.shaper.as_ref().expect("shaper in path");
    shaper.control.start_capture();
    let marker = format!("SLIPSTREAM-PLAINTEXT-MARKER-{:016x}", rand_u64());
    let body = format!(r#"{{"model":"slipstream","stream":true,"messages":[{{"role":"user","content":"{marker}"}}]}}"#);
    let fwd = r.use_side.bind;
    let auth = format!("Bearer {token}");
    let resp = send(
        fwd,
        &request("POST", "/v1/chat/completions", &fwd.to_string(), &[("Authorization", auth.as_str())], body.as_bytes()),
    )
    .await;
    assert_eq!(resp.status(), 200, "{}", resp.head());
    let text = resp.body_text();
    assert!(text.contains("data: [DONE]"), "{text}");
    // the mock really received the marker (so it did travel, encrypted)
    assert!(r.mock.requests().iter().any(|q| q.body_len == body.len()));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let cap = shaper.control.take_capture();
    let total: usize = cap.iter().map(|p| p.len()).sum();
    assert!(cap.len() >= 4 && total > body.len(), "capture too small: {} datagrams, {total} bytes", cap.len());
    let needles: Vec<(&str, Vec<u8>)> = vec![
        ("prompt marker", marker.as_bytes().to_vec()),
        ("request line", b"POST /v1/chat/completions".to_vec()),
        ("JSON body key", br#""messages""#.to_vec()),
        ("SSE terminator", b"data: [DONE]".to_vec()),
        ("first SSE event", text.lines().find(|l| l.starts_with("data: {")).unwrap_or("data: {").as_bytes().to_vec()),
        ("local token", token.as_bytes().to_vec()),
        ("Authorization header", b"Bearer".to_vec()),
    ];
    for (what, n) in &needles {
        for (i, p) in cap.iter().enumerate() {
            assert!(!contains(p, n), "{what} in plaintext in datagram {i} ({} bytes)", p.len());
        }
    }
    // positive control: the capture does see plaintext when there is some
    shaper.control.start_capture();
    let probe = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    probe.send_to(marker.as_bytes(), shaper.addr).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(shaper.control.take_capture().iter().any(|p| contains(p, marker.as_bytes())), "capture is blind");
    println!("wire_capture datagrams={} bytes={total} needles={} plaintext_hits=0", cap.len(), needles.len());
}

fn rand_u64() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64 ^ std::process::id() as u64
}
