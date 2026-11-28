//! Regression tests for the M1 security review of the forwarder (commit d2f158d):
//! request smuggling through `ReqHead.target`, share-side body cap, light-route
//! admission, local bearer token + connection cap + body idle timeout, `[::1]` bind,
//! idle-stream / handshake limits, `/props` sanitizing. See bench/RESULTS.md
//! "v0.6 M1 security fixes".

mod support;

use std::time::{Duration, Instant};

use p2p_forward::admission::AdmissionLimits;
use p2p_forward::proto::{BodyKind, FrameType, MAX_BODY};
use p2p_forward::{load_or_create_identity, start_use, LocalLimits, ShareLimits, UseConfig};
use p2p_libp2p::paired::PairedConfig;
use support::{chat_body, raw_peer, read_response, request, rig, send, RigOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const NOT_AVAILABLE: &str = "Not available through a shared Mac";

fn h(k: &str, v: &str) -> (String, String) {
    (k.to_string(), v.to_string())
}

/// Finding 1 (CRITICAL): a malicious paired peer injected CR/LF into `ReqHead.target`
/// and got three upstream requests (incl. `POST /slots/0?action=save`) out of one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn smuggled_target_never_reaches_llama_server() {
    let r = rig(RigOptions::default()).await;
    let raw = raw_peer(&r).await;
    let exploit = "/health?a HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\n\r\n\
        POST /slots/0?action=save HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: 20\r\n\r\n\
        {\"filename\":\"x.bin\"}GET /health?b";
    for (what, target) in [
        ("verified exploit", exploit.to_string()),
        ("CRLF", "/health\r\nX: 1".to_string()),
        ("LF", "/health?\nPOST /slots/0".to_string()),
        ("space", "/health HTTP/1.1".to_string()),
        ("NEL U+0085", "/health?\u{0085}POST".to_string()),
        ("NUL", "/health\0".to_string()),
        ("TAB", "/health\t".to_string()),
    ] {
        let resp = raw.exchange("GET", &target, vec![], BodyKind::None, &[]).await.unwrap();
        assert!(resp.starts_with("HTTP/1.1 403"), "{what}: {resp}");
        assert!(resp.contains(NOT_AVAILABLE), "{what}");
    }
    assert!(r.mock.requests().is_empty(), "smuggled bytes reached upstream: {:?}", r.mock.requests());

    // Valid bytes with a hostile query: forwarded as exactly one request with the
    // constant path; the query is dropped.
    let resp = raw
        .exchange(
            "POST",
            "/v1/chat/completions?n=2&x=%0d%0aPOST%20/slots/0?action=save",
            vec![h("content-type", "application/json")],
            BodyKind::Length(chat_body("plain").len() as u64),
            &[chat_body("plain")],
        )
        .await
        .unwrap();
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    let seen = r.mock.requests();
    assert_eq!(seen.len(), 1, "exactly one upstream request: {seen:?}");
    assert_eq!((seen[0].method.as_str(), seen[0].target.as_str()), ("POST", "/v1/chat/completions"));
    // GET /slots keeps only fail_on_no_slot
    raw.exchange("GET", "/slots?fail_on_no_slot=1", vec![], BodyKind::None, &[]).await.unwrap();
    raw.exchange("GET", "/slots?id_slot=0&action=save", vec![], BodyKind::None, &[]).await.unwrap();
    let targets: Vec<String> = r.mock.requests().iter().map(|s| s.target.clone()).collect();
    assert_eq!(targets, vec!["/v1/chat/completions", "/slots?fail_on_no_slot=1", "/slots"]);
}

/// Finding 1, use side: odd target bytes are refused locally (400 from the HTTP parser
/// or 403 from the byte check) and nothing leaves the Mac.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn use_side_rejects_target_bytes_outside_visible_ascii() {
    let r = rig(RigOptions::default()).await;
    let fwd = r.use_side.bind;
    let host = fwd.to_string();
    for target in [&b"/health?\xc2\x85x"[..], b"/health?\x01", b"/health?\x7f", b"/health?\xff"] {
        let mut req = b"GET ".to_vec();
        req.extend_from_slice(target);
        req.extend_from_slice(format!(" HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes());
        let resp = send(fwd, &req).await;
        assert!(matches!(resp.status(), 400 | 403), "{target:?}: {}", resp.head());
    }
    // valid request with a query: the use side itself sends the constant path
    let resp = send(fwd, &request("GET", "/health?x=1", &host, &[], b"")).await;
    assert_eq!(resp.status(), 200);
    let seen = r.mock.requests();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].target, "/health");
}

/// Finding 2: the share side enforces the 32 MiB cap itself (declared and streamed).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn share_side_caps_request_bodies() {
    let r = rig(RigOptions {
        share_limits: ShareLimits {
            max_body: 64 * 1024,
            ..ShareLimits::default()
        },
        ..RigOptions::default()
    })
    .await;
    let raw = raw_peer(&r).await;
    // declared too large: refused before connecting upstream (default cap = 32 MiB)
    assert_eq!(ShareLimits::default().max_body, MAX_BODY);
    let resp = raw
        .exchange("POST", "/v1/chat/completions", vec![], BodyKind::Length(64 * 1024 + 1), &[])
        .await
        .unwrap();
    assert!(resp.starts_with("HTTP/1.1 413"), "{resp}");
    assert!(r.mock.requests().is_empty());
    // chunked (undeclared) body over the cap: counted, upstream dropped, 413
    let frames: Vec<Vec<u8>> = (0..8).map(|_| vec![b'a'; 16 * 1024]).collect();
    let resp = raw
        .exchange("POST", "/v1/chat/completions", vec![], BodyKind::Chunked, &frames)
        .await
        .unwrap_or_default();
    assert!(resp.is_empty() || resp.starts_with("HTTP/1.1 413"), "{resp}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    for s in r.mock.requests() {
        assert!(s.body_len <= 64 * 1024, "upstream got {} body bytes", s.body_len);
    }
    // a Length body longer than declared is cut off as well (never forwarded)
    let resp = raw
        .exchange("POST", "/tokenize", vec![], BodyKind::Length(4), &[b"abcdefgh".to_vec()])
        .await
        .unwrap_or_default();
    assert!(!resp.starts_with("HTTP/1.1 200"), "{resp}");
    assert!(r.share.stats.lock().unwrap().body_too_large >= 2);
}

/// Finding 3: light routes have their own admission (4 per peer + global cap).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fifth_concurrent_light_request_gets_503() {
    let r = rig(RigOptions {
        admission: AdmissionLimits {
            light_wait: Duration::from_millis(200),
            ..AdmissionLimits::default()
        },
        ..RigOptions::default()
    })
    .await;
    let fwd = r.use_side.bind;
    let host = fwd.to_string();
    let slow = request("POST", "/tokenize?delay_ms=1500", &host, &[], br#"{"content":"x"}"#);
    let mut joins = Vec::new();
    for _ in 0..4 {
        let slow = slow.clone();
        joins.push(tokio::spawn(async move { send(fwd, &slow).await }));
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    let t = Instant::now();
    let fifth = send(fwd, &request("GET", "/health", &host, &[], b"")).await;
    assert_eq!(fifth.status(), 503, "{}", fifth.head());
    assert!(fifth.head().contains("Retry-After: 5"));
    assert!(t.elapsed() < Duration::from_millis(1200), "refused after light_wait, not queued forever");
    for j in joins {
        assert_eq!(j.await.unwrap().status(), 200);
    }
    // places free again
    assert_eq!(send(fwd, &request("GET", "/health", &host, &[], b"")).await.status(), 200);
}

/// Finding 4: optional per-install bearer token on the local listener.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_token_required_when_configured() {
    let token = "4f1c0e0b9a7d6c5b4a39281706f5e4d3c2b1a09f8e7d6c5b4a3928170605f4e3".to_string();
    let r = rig(RigOptions {
        token: Some(token.clone()),
        ..RigOptions::default()
    })
    .await;
    let fwd = r.use_side.bind;
    let host = fwd.to_string();
    let good = format!("Bearer {token}");
    for (what, extra) in [
        ("none", vec![]),
        ("wrong", vec![("Authorization", "Bearer 0000000000000000000000000000000000000000000000000000000000000000")]),
        ("no scheme", vec![("Authorization", token.as_str())]),
        ("duplicate", vec![("Authorization", "Bearer nope"), ("Authorization", good.as_str())]),
    ] {
        let resp = send(fwd, &request("POST", "/v1/chat/completions", &host, &extra, &chat_body("plain"))).await;
        assert_eq!(resp.status(), 401, "{what}: {}", resp.head());
        assert!(resp.head().contains("WWW-Authenticate: Bearer"));
    }
    assert!(r.mock.requests().is_empty());
    let resp = send(fwd, &request("POST", "/v1/chat/completions?n=3", &host, &[("Authorization", &good)], &chat_body("plain"))).await;
    assert_eq!(resp.status(), 200);
    let seen = r.mock.requests();
    assert_eq!(seen.len(), 1);
    assert!(!seen[0].headers.iter().any(|(k, _)| k == "authorization"), "token stripped before forwarding");
    assert_eq!(r.use_side.stats.lock().unwrap().unauthorized, 4);
}

/// Finding 4: concurrent local connections capped; slow body uploads time out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_connection_cap_and_body_idle_timeout() {
    let r = rig(RigOptions {
        local: LocalLimits {
            max_connections: 2,
            body_idle_timeout: Duration::from_millis(300),
        },
        ..RigOptions::default()
    })
    .await;
    let fwd = r.use_side.bind;
    let host = fwd.to_string();
    // two idle connections hold both places
    let a = TcpStream::connect(fwd).await.unwrap();
    let b = TcpStream::connect(fwd).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let third = send(fwd, &request("GET", "/health", &host, &[], b"")).await;
    assert_eq!(third.status(), 503, "{}", third.head());
    drop((a, b));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(send(fwd, &request("GET", "/health", &host, &[], b"")).await.status(), 200);
    // body stalls after 10 of 100 bytes → 408 after the idle timeout
    let mut sock = TcpStream::connect(fwd).await.unwrap();
    sock.write_all(
        format!("POST /v1/chat/completions HTTP/1.1\r\nHost: {host}\r\nContent-Length: 100\r\n\r\n0123456789").as_bytes(),
    )
    .await
    .unwrap();
    let t = Instant::now();
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), sock.read_to_end(&mut out)).await;
    let text = String::from_utf8_lossy(&out);
    assert!(text.starts_with("HTTP/1.1 408"), "{text}");
    assert!(t.elapsed() < Duration::from_secs(3));
    assert_eq!(r.use_side.stats.lock().unwrap().connections_over_cap, 1);
}

/// Finding 5: the use side serves `[::1]` too, and refuses to start if it is taken.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn binds_ipv6_loopback_too_and_refuses_if_taken() {
    let r = rig(RigOptions::default()).await;
    let v6 = r.use_side.bind_companion;
    assert_eq!(v6.ip(), std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST));
    assert_eq!(v6.port(), r.use_side.bind.port());
    let resp = send(v6, &request("GET", "/health", &format!("[::1]:{}", v6.port()), &[], b"")).await;
    assert_eq!(resp.status(), 200);
    let resp = send(v6, &request("GET", "/health", "localhost", &[], b"")).await;
    assert_eq!(resp.status(), 200);

    // another local process already holds [::1]:<port> → refuse to start
    let squatter = std::net::TcpListener::bind("[::1]:0").unwrap();
    let port = squatter.local_addr().unwrap().port();
    let identity = load_or_create_identity(&r.dir.path().join("macbook2")).unwrap();
    let res = start_use(
        identity,
        UseConfig {
            peer: r.share.dial_addr(),
            trust: r.use_trust.clone(),
            node: r.use_node.clone(),
            bind: format!("127.0.0.1:{port}").parse().unwrap(),
            paired: PairedConfig::default(),
            spare_stream: false,
            token: None,
            local: LocalLimits::default(),
            discovery: None,
            app_origin: None,
            verbose: false,
        },
    )
    .await;
    let err = res.err().expect("must refuse when [::1] is taken");
    assert!(err.to_string().contains("::1"), "{err}");
    // and the IPv4 side was not left bound
    std::net::TcpListener::bind(format!("127.0.0.1:{port}")).expect("127.0.0.1 released");
}

/// Finding 6: the share side bounds the handshake and idle (pre-ReqHead) streams.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_streams_capped_and_handshake_bounded() {
    let r = rig(RigOptions {
        spare_stream: false,
        share_limits: ShareLimits {
            handshake_timeout: Duration::from_millis(300),
            first_frame_timeout: Duration::from_millis(300),
            idle_streams_per_peer: 2,
            ..ShareLimits::default()
        },
        ..RigOptions::default()
    })
    .await;
    let raw = raw_peer(&r).await;
    // three handshaken streams that never send a ReqHead: two may stay idle
    let mut chans = Vec::new();
    for _ in 0..3 {
        chans.push(raw.channel().await);
    }
    tokio::time::sleep(Duration::from_millis(900)).await;
    let mut ok = 0;
    for (mut rd, mut wr) in chans {
        let head = p2p_forward::proto::ReqHead {
            method: "GET".into(),
            target: "/health".into(),
            headers: vec![],
            body: BodyKind::None,
        };
        if wr.send_json(FrameType::ReqHead, &head).await.is_err() {
            continue;
        }
        let _ = wr.send(FrameType::ReqBody, &[]).await;
        if let Ok(Ok(resp)) = tokio::time::timeout(Duration::from_secs(3), read_response(&mut rd)).await {
            if resp.starts_with("HTTP/1.1 200") {
                ok += 1;
            }
        }
    }
    assert_eq!(ok, 2, "exactly the two idle places survive");
    assert_eq!(r.share.stats.lock().unwrap().idle_streams_refused, 1);

    // a stream that never finishes the SealedStream handshake is dropped
    let mut control = raw.control.clone();
    let stream = control.open_stream(raw.peer, p2p_forward::proto::OPENAI_PROTOCOL_V1).await.unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(r.share.stats.lock().unwrap().handshake_timeouts >= 1);
    drop(stream);
    // the forwarder still works afterwards
    let fwd = r.use_side.bind;
    assert_eq!(send(fwd, &request("GET", "/health", &fwd.to_string(), &[], b"")).await.status(), 200);
}

/// Finding 6: `/props` from a modified use side is sanitized by the share side.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn props_sanitized_on_share_side() {
    let r = rig(RigOptions::default()).await;
    let raw = raw_peer(&r).await;
    let resp = raw.exchange("GET", "/props", vec![], BodyKind::None, &[]).await.unwrap();
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    assert!(!resp.contains("model_path") && !resp.contains("/Users") && !resp.contains("chat_template"), "{resp}");
    assert!(resp.contains(r#""n_ctx":8192"#));
}
