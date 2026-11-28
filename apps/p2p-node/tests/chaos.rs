//! v0.6 M5 chaos suite (plan §7 `chaos.rs`) on one Mac.
//!
//! Every streamed response must either be **byte-identical** to the mock llama-server's
//! deterministic stream, or end with the documented error event: a prefix of the expected
//! stream cut at a chunk (= SSE event) boundary, then one `data: {"error":{…
//! "slipstream_code":"peer_lost"…}}` chunk and the terminating chunk
//! ([`p2p_forward::client::peer_lost_tail`]). No request may hang; after the disturbance the
//! use side must re-dial by itself and serve identical streams again.
//!
//! 1. 2 % packet loss + 20 ms jitter (userspace UDP shaper, both directions)
//! 2. blackhole mid-stream (big Mac gone), then recovery
//! 3. SIGSTOP of a real `slipstream-node share` process mid-stream (= the big Mac sleeps),
//!    then SIGCONT (wakes) — with the app's session code on the use side

#[path = "../../../crates/p2p-forward/tests/support/mod.rs"]
mod support;

use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use p2p_forward::client::peer_lost_tail;
use p2p_forward::mock_llama::{self, Variant};
use support::{chat_body, request, rig, RigOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Identical,
    PeerLost { events_before: usize },
}

/// Classify a response against the expected bytes (panics on anything else).
fn classify(got: &[u8], expected: &[u8], what: &str) -> Outcome {
    if got == expected {
        return Outcome::Identical;
    }
    let tail = peer_lost_tail();
    assert!(
        got.ends_with(&tail),
        "{what}: neither identical nor ended with the peer_lost event ({} of {} bytes): …{}",
        got.len(),
        expected.len(),
        String::from_utf8_lossy(&got[got.len().saturating_sub(200)..])
    );
    let prefix = &got[..got.len() - tail.len()];
    assert!(expected.starts_with(prefix), "{what}: the bytes before the error event differ from the stream");
    // the cut is at a chunk boundary: the prefix ends with an event's "\n\n\r\n" (or is the head)
    let head_end = p2p_forward::http::find_head_end(expected).unwrap();
    assert!(prefix.len() == head_end || prefix.ends_with(b"\n\n\r\n"), "{what}: cut inside an event");
    Outcome::PeerLost {
        events_before: prefix.windows(6).filter(|w| w == b"data: ").count(),
    }
}

/// One request with an overall deadline (no hangs); returns the raw bytes.
async fn fetch(addr: SocketAddr, req: &[u8], deadline: Duration) -> Vec<u8> {
    tokio::time::timeout(deadline, async {
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(req).await.unwrap();
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out).await;
        out
    })
    .await
    .unwrap_or_else(|_| panic!("request hung for more than {deadline:?}"))
}

fn variant(i: usize) -> (&'static str, Variant) {
    match i % 3 {
        0 => ("plain", Variant::Plain),
        1 => ("json", Variant::Json),
        _ => ("tool", Variant::Tool),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loss_and_jitter_streams_are_identical_or_end_with_the_error_event() {
    let r = rig(RigOptions {
        rtt: Some(Duration::from_millis(4)),
        ..RigOptions::default()
    })
    .await;
    let shaper = &r.shaper.as_ref().unwrap().control;
    shaper.set_loss(0.02);
    shaper.set_jitter(Duration::from_millis(20));
    let fwd = r.use_side.bind;
    let host = fwd.to_string();
    let (mut identical, mut lost) = (0, 0);
    let t0 = Instant::now();
    for i in 0..12 {
        let (name, v) = variant(i);
        let req = request(
            "POST",
            "/v1/chat/completions?n=120&gap_us=2000",
            &host,
            &[("Content-Type", "application/json")],
            &chat_body(name),
        );
        let got = fetch(fwd, &req, Duration::from_secs(60)).await;
        match classify(&got, &mock_llama::expected_stream(v, 120), &format!("stream {i} ({name})")) {
            Outcome::Identical => identical += 1,
            Outcome::PeerLost { .. } => lost += 1,
        }
    }
    let dropped = shaper.dropped.load(std::sync::atomic::Ordering::Relaxed);
    let forwarded = shaper.forwarded.load(std::sync::atomic::Ordering::Relaxed);
    println!(
        "chaos loss2_jitter20 identical={identical} peer_lost={lost} packets_dropped={dropped} forwarded={forwarded} total_s={:.1}",
        t0.elapsed().as_secs_f64()
    );
    assert!(dropped > 0, "the shaper dropped nothing");
    // QUIC retransmits: at 2 % loss the connection stays up, so every stream completes
    assert_eq!(identical, 12, "streams ended early at 2 % loss");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blackhole_mid_stream_ends_with_the_error_event_then_recovers() {
    let r = rig(RigOptions {
        rtt: Some(Duration::from_millis(4)),
        ..RigOptions::default()
    })
    .await;
    let shaper = r.shaper.as_ref().unwrap().control.clone();
    let fwd = r.use_side.bind;
    let host = fwd.to_string();
    let req = request(
        "POST",
        "/v1/chat/completions?n=2000&gap_us=20000",
        &host,
        &[("Content-Type", "application/json")],
        &chat_body("plain"),
    );
    let expected = mock_llama::expected_stream(Variant::Plain, 2000);
    let s2 = shaper.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        s2.set_blackhole(true);
    });
    let t0 = Instant::now();
    let got = fetch(fwd, &req, Duration::from_secs(20)).await;
    let out = classify(&got, &expected, "blackholed stream");
    println!("chaos blackhole outcome={out:?} ended_after_s={:.2}", t0.elapsed().as_secs_f64());
    assert!(matches!(out, Outcome::PeerLost { events_before } if events_before > 0));
    // while gone: a clear 503 (no hang), then recovery and identical streams again
    let resp = fetch(fwd, &request("GET", "/health", &host, &[], b""), Duration::from_secs(10)).await;
    assert!(resp.starts_with(b"HTTP/1.1 503"), "{}", String::from_utf8_lossy(&resp));
    shaper.set_blackhole(false);
    let t1 = Instant::now();
    assert!(r.use_side.wait_connected(Duration::from_secs(15)).await, "no re-dial after recovery");
    println!("chaos blackhole redial_s={:.2}", t1.elapsed().as_secs_f64());
    let req = request("POST", "/v1/chat/completions?n=50", &host, &[("Content-Type", "application/json")], &chat_body("tool"));
    let got = fetch(fwd, &req, Duration::from_secs(20)).await;
    assert_eq!(classify(&got, &mock_llama::expected_stream(Variant::Tool, 50), "after recovery"), Outcome::Identical);
}

struct Proc(Child);
impl Drop for Proc {
    fn drop(&mut self) {
        let pid = self.0.id().to_string();
        let _ = Command::new("kill").args(["-CONT", &pid]).status();
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sigstop_of_the_share_process_then_sigcont() {
    use p2p_node::session::{Node, ShareOpts, ShareSession, UseOpts, UseSession, UseState};
    let tmp = tempfile::tempdir().unwrap();
    let mock = mock_llama::start("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let big_dir = tmp.path().join("big");
    let book = Node::open(&tmp.path().join("book")).unwrap();
    // pair in-process, then hand the big Mac's data dir to a real share process
    let port = {
        let big = Node::open(&big_dir).unwrap();
        let mut o = ShareOpts::new(mock.addr);
        o.listen = Some("/ip4/127.0.0.1/udp/0/quic-v1".into());
        o.announce = false;
        let share = ShareSession::start(big, o).await.unwrap();
        let code = share.open_pairing().code;
        p2p_node::session::pair_with_target(&book, &share.dial_addr(), &[], &code, Some("MacBook".into()))
            .await
            .unwrap();
        share.port().unwrap()
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let addr_file = tmp.path().join("addr");
    let share = Proc(
        Command::new(env!("CARGO_BIN_EXE_slipstream-node"))
            .args(["share", "--listen", &format!("/ip4/127.0.0.1/udp/{port}/quic-v1"), "--forward"])
            .arg(format!("http://{}", mock.addr))
            .arg("--data-dir")
            .arg(&big_dir)
            .args(["--name", "Studio", "--no-announce", "--addr-file"])
            .arg(&addr_file)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let t0 = Instant::now();
    while std::fs::read_to_string(&addr_file).map(|s| s.trim().is_empty()).unwrap_or(true) {
        assert!(t0.elapsed() < Duration::from_secs(10), "share process never listened");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let big_peer = book.store.list()[0].peer_id.clone();
    let bind: SocketAddr = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let mut o = UseOpts::new(big_peer, bind);
    o.discovery = false;
    o.wake = p2p_node::wake::WakeTargets::Off;
    let u = UseSession::start(book.clone(), o).unwrap();
    let t0 = Instant::now();
    while u.state() != UseState::Connected {
        assert!(t0.elapsed() < Duration::from_secs(10), "never connected: {:?}", u.state());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let host = bind.to_string();
    let req = request(
        "POST",
        "/v1/chat/completions?n=2000&gap_us=20000",
        &host,
        &[("Content-Type", "application/json")],
        &chat_body("plain"),
    );
    let pid = share.0.id().to_string();
    let p2 = pid.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        let _ = Command::new("kill").args(["-STOP", &p2]).status();
    });
    let t_stop = Instant::now();
    let got = fetch(bind, &req, Duration::from_secs(20)).await;
    let out = classify(&got, &mock_llama::expected_stream(Variant::Plain, 2000), "SIGSTOPped share");
    let detected = t_stop.elapsed();
    println!("chaos sigstop outcome={out:?} ended_after_s={:.2}", detected.as_secs_f64());
    assert!(matches!(out, Outcome::PeerLost { .. }));
    assert!(detected <= Duration::from_secs(12), "detection took {detected:?}");
    // while the share is stopped: the editor port answers 503 (waking), never hangs
    let resp = fetch(bind, &request("GET", "/health", &host, &[], b""), Duration::from_secs(10)).await;
    assert!(resp.starts_with(b"HTTP/1.1 503"), "{}", String::from_utf8_lossy(&resp));
    let _ = Command::new("kill").args(["-CONT", &pid]).status();
    let t_cont = Instant::now();
    while u.state() != UseState::Connected {
        assert!(t_cont.elapsed() < Duration::from_secs(20), "no re-dial after SIGCONT: {:?}", u.state());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    println!("chaos sigcont redial_s={:.2}", t_cont.elapsed().as_secs_f64());
    let req = request("POST", "/v1/chat/completions?n=50", &host, &[("Content-Type", "application/json")], &chat_body("json"));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let got = fetch(bind, &req, Duration::from_secs(20)).await;
        if got.starts_with(b"HTTP/1.1 503") && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        }
        assert_eq!(classify(&got, &mock_llama::expected_stream(Variant::Json, 50), "after SIGCONT"), Outcome::Identical);
        break;
    }
    drop(u);
    drop(share);
}
