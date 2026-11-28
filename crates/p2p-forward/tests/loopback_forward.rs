//! M1 loopback tests of the allowlisted forwarder (plan §7 `loopback_forward.rs`):
//! byte-identical responses vs direct, 403s on both ends, Host/Origin guards, header
//! stripping, `/slots` sanitizing, body cap, trust gate, admission, status RPC.

mod support;

use std::time::Duration;

use futures::StreamExt;
use p2p_crypto::StreamRole;
use p2p_forward::mock_llama::{self, Variant, SECRET_PROMPT};
use p2p_forward::proto::{channel_context, open_channel, BodyKind, FrameType, ReqHead, OPENAI_PROTOCOL_V1};
use p2p_forward::status::{StatusReply, StatusRequest};
use p2p_forward::{load_or_create_identity, start_use, UseConfig};
use p2p_libp2p::paired::{build_paired_swarm, Control, PairedConfig};
use support::{chat_body, request, rig, send, RigOptions};

const NOT_AVAILABLE: &str = "Not available through a shared Mac";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sse_byte_identical_plain_json_tool_and_models() {
    let r = rig(RigOptions::default()).await;
    let fwd = r.use_side.bind;
    let fwd_host = fwd.to_string();
    let direct_host = r.mock.addr.to_string();
    for (name, variant) in [("plain", Variant::Plain), ("json", Variant::Json), ("tool", Variant::Tool)] {
        let body = chat_body(name);
        let target = "/v1/chat/completions?n=200&gap_us=200";
        for _ in 0..3 {
            let direct = send(r.mock.addr, &request("POST", target, &direct_host, &[("Content-Type", "application/json")], &body)).await;
            let forwarded = send(fwd, &request("POST", target, &fwd_host, &[("Content-Type", "application/json")], &body)).await;
            assert_eq!(direct.bytes, mock_llama::expected_stream(variant, 200), "{name}: mock not deterministic");
            assert_eq!(forwarded.bytes, direct.bytes, "{name}: forwarded response differs from direct");
        }
    }
    // non-streamed + light GETs + POST tokenize: identical as well
    for (method, target, body) in [
        ("GET", "/v1/models", &b""[..]),
        ("GET", "/health", b""),
        ("GET", "/props", b""),
        ("GET", "/metrics", b""),
        ("POST", "/tokenize", br#"{"content":"abc"}"#),
        ("POST", "/detokenize", br#"{"tokens":[1]}"#),
        ("POST", "/apply-template", br#"{"messages":[]}"#),
        ("POST", "/v1/completions", br#"{"prompt":"x","stream":false}"#),
        ("POST", "/v1/chat/completions", br#"{"messages":[],"stream":false}"#),
    ] {
        let direct = send(r.mock.addr, &request(method, target, &direct_host, &[], body)).await;
        let forwarded = send(fwd, &request(method, target, &fwd_host, &[], body)).await;
        assert_eq!(direct.status(), 200, "{method} {target}");
        assert_eq!(forwarded.bytes, direct.bytes, "{method} {target}");
    }
    let s = r.use_side.stats.lock().unwrap().clone();
    assert_eq!(s.connections_established, 1, "one persistent connection");
    assert!(s.spare_hits > 0, "pre-opened spare stream used");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refused_requests_never_reach_llama_server() {
    let r = rig(RigOptions::default()).await;
    let fwd = r.use_side.bind;
    let host = fwd.to_string();
    let cases: Vec<(Vec<u8>, &str)> = vec![
        (request("POST", "/slots/0?action=save", &host, &[], br#"{"filename":"x.bin"}"#), "slot save"),
        (request("POST", "/slots/0?action=restore", &host, &[], br#"{"filename":"../../etc"}"#), "slot restore"),
        (request("POST", "/slots/0?action=erase", &host, &[], b""), "slot erase"),
        (request("POST", "/props", &host, &[], br#"{"x":1}"#), "props write"),
        (request("GET", "/slots/0", &host, &[], b""), "slot detail"),
        (request("GET", "/", &host, &[], b""), "web ui"),
        (request("POST", "/infill", &host, &[], b"{}"), "unknown path"),
        (request("POST", "/completion", &host, &[], b"{}"), "non-OpenAI completion"),
        (request("POST", "/v1/chat%2Fcompletions", &host, &[], b"{}"), "encoded path"),
        (request("DELETE", "/slots", &host, &[], b""), "method"),
        (request("POST", "/v1/chat/completions", "evil.example:8080", &[], &chat_body("plain")), "bad Host"),
        (request("POST", "/v1/chat/completions", &format!("127.0.0.1:{}", fwd.port() ^ 1), &[], &chat_body("plain")), "wrong port in Host"),
        (request("POST", "/v1/chat/completions", &host, &[("Origin", "https://evil.example")], &chat_body("plain")), "Origin"),
        (request("POST", "/v1/chat/completions", &host, &[("Origin", &format!("http://{host}"))], &chat_body("plain")), "same-host Origin"),
        (request("POST", "/v1/chat/completions", &host, &[("Origin", "null"), ("Sec-Fetch-Site", "cross-site")], &chat_body("plain")), "sandboxed frame"),
    ];
    for (req, what) in &cases {
        let resp = send(fwd, req).await;
        assert_eq!(resp.status(), 403, "{what}: {}", resp.head());
        assert!(resp.body_text().contains(NOT_AVAILABLE), "{what}");
    }
    // missing Host
    let resp = send(fwd, b"GET /health HTTP/1.1\r\nConnection: close\r\n\r\n").await;
    assert_eq!(resp.status(), 403);
    assert!(r.mock.requests().is_empty(), "nothing reached llama-server: {:?}", r.mock.requests());
    assert_eq!(r.share.stats.lock().unwrap().requests, 0, "refused before opening a stream");
    // a null Origin without cross-site marking (e.g. a non-browser tool) is allowed
    let ok = send(fwd, &request("GET", "/health", &host, &[("Origin", "null")], b"")).await;
    assert_eq!(ok.status(), 200);
    let ok = send(fwd, &request("GET", "/health", "localhost", &[], b"")).await;
    assert_eq!(ok.status(), 200);
}

/// A modified use side that skips its own allowlist is still refused by the share side.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn share_side_enforces_allowlist_and_strips_headers_itself() {
    let r = rig(RigOptions::default()).await;
    let identity = load_or_create_identity(&r.dir.path().join("macbook")).unwrap();
    let local = identity.public().to_peer_id();
    let mut swarm = build_paired_swarm(identity, &PairedConfig::default());
    swarm.dial(r.share.dial_addr()).unwrap();
    let control = swarm.behaviour().stream.new_control();
    tokio::spawn(async move {
        loop {
            swarm.select_next_some().await;
        }
    });
    let peer = r.share.peer_id;
    let raw = |m: &str, t: &str, h: Vec<(String, String)>| raw_request(control.clone(), local, peer, m.to_string(), t.to_string(), h);
    for (m, t) in [("POST", "/slots/0?action=save"), ("POST", "/props"), ("GET", "/slots/1"), ("POST", "/infill")] {
        let resp = raw(m, t, vec![]).await;
        assert!(resp.starts_with("HTTP/1.1 403"), "{m} {t}: {resp}");
        assert!(resp.contains(NOT_AVAILABLE));
    }
    assert!(r.mock.requests().is_empty(), "share side let a refused request through");
    // header stripping on the share side, too
    let resp = raw(
        "GET",
        "/health",
        vec![
            ("authorization".into(), "Bearer sk-secret".into()),
            ("cookie".into(), "session=1".into()),
            ("connection".into(), "x-hop".into()),
            ("x-hop".into(), "1".into()),
            ("host".into(), "evil.example".into()),
            ("x-editor".into(), "kilo".into()),
        ],
    )
    .await;
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    let seen = r.mock.requests();
    let h = &seen.last().unwrap().headers;
    let names: Vec<&str> = h.iter().map(|(k, _)| k.as_str()).collect();
    assert!(!names.contains(&"authorization") && !names.contains(&"cookie") && !names.contains(&"x-hop"), "{h:?}");
    assert!(h.iter().any(|(k, v)| k == "host" && v == &r.mock.addr.to_string()), "{h:?}");
    assert!(h.iter().any(|(k, v)| k == "x-editor" && v == "kilo"));
    drop(control);
}

async fn raw_request(
    mut control: Control,
    local: libp2p::PeerId,
    peer: libp2p::PeerId,
    method: String,
    target: String,
    headers: Vec<(String, String)>,
) -> String {
    let head = ReqHead { method, target, headers, body: BodyKind::None };
    let stream = control.open_stream(peer, OPENAI_PROTOCOL_V1).await.unwrap();
    let ctx = channel_context(&local, &peer, &OPENAI_PROTOCOL_V1);
    let (mut rd, mut wr) = open_channel(stream, StreamRole::Initiator, &ctx).await.unwrap();
    wr.send_json(FrameType::ReqHead, &head).await.unwrap();
    wr.send(FrameType::ReqBody, &[]).await.unwrap();
    let mut out = Vec::new();
    while let Some(f) = rd.next().await.unwrap() {
        if f.kind == FrameType::RespBody && f.payload.is_empty() {
            break;
        }
        out.extend_from_slice(&f.payload);
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn credentials_stripped_and_slots_sanitized() {
    let r = rig(RigOptions::default()).await;
    let fwd = r.use_side.bind;
    let host = fwd.to_string();
    let resp = send(
        fwd,
        &request(
            "POST",
            "/v1/chat/completions?n=4",
            &host,
            &[
                ("Authorization", "Bearer sk-local-key"),
                ("Cookie", "a=b"),
                ("Proxy-Authorization", "Basic x"),
                ("Keep-Alive", "timeout=5"),
                ("Content-Type", "application/json"),
                ("X-Title", "Kilo Code"),
            ],
            &chat_body("plain"),
        ),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let seen = r.mock.requests();
    let h = &seen[0].headers;
    for k in ["authorization", "cookie", "proxy-authorization", "keep-alive"] {
        assert!(!h.iter().any(|(n, _)| n == k), "{k} forwarded: {h:?}");
    }
    assert!(h.iter().any(|(n, v)| n == "x-title" && v == "Kilo Code"));
    assert!(h.iter().any(|(n, v)| n == "connection" && v == "close"));
    assert_eq!(seen[0].body_sha256, hex::encode(<sha2::Sha256 as sha2::Digest>::digest(chat_body("plain"))));

    let slots = send(fwd, &request("GET", "/slots", &host, &[], b"")).await;
    assert_eq!(slots.status(), 200);
    let text = slots.body_text();
    assert!(!text.contains("SECRET"), "{text}");
    assert!(!text.contains(SECRET_PROMPT));
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        v,
        serde_json::json!([{"id":0,"n_ctx":8192,"is_processing":true,"phase":"prompt",
            "prompt_progress":{"total":1000,"cache":0,"processed":400,"time_ms":1234.5}}])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bodies_capped_chunked_and_expect_continue() {
    let r = rig(RigOptions::default()).await;
    let fwd = r.use_side.bind;
    let host = fwd.to_string();
    // > 32 MiB declared: refused locally, nothing sent
    let big = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: {host}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        32 * 1024 * 1024 + 1
    );
    let resp = send(fwd, big.as_bytes()).await;
    assert_eq!(resp.status(), 413);
    // 4 MiB body forwarded intact (streamed in 16 KiB frames)
    let mut body = br#"{"stream":false,"messages":[{"role":"user","content":""#.to_vec();
    body.extend(std::iter::repeat(b'a').take(4 * 1024 * 1024));
    body.extend_from_slice(br#""}]}"#);
    let resp = send(fwd, &request("POST", "/v1/chat/completions", &host, &[("Expect", "100-continue")], &body)).await;
    assert!(resp.head().starts_with("HTTP/1.1 100 Continue"), "{}", resp.head());
    let seen = r.mock.requests();
    assert_eq!(seen.last().unwrap().body_len, body.len());
    assert_eq!(seen.last().unwrap().body_sha256, hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&body)));
    // chunked request body
    let payload = chat_body("tool");
    let mut req = format!(
        "POST /v1/chat/completions?n=8 HTTP/1.1\r\nHost: {host}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
    )
    .into_bytes();
    for part in payload.chunks(7) {
        req.extend_from_slice(format!("{:x}\r\n", part.len()).as_bytes());
        req.extend_from_slice(part);
        req.extend_from_slice(b"\r\n");
    }
    req.extend_from_slice(b"0\r\n\r\n");
    let resp = send(fwd, &req).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes, mock_llama::expected_stream(Variant::Tool, 8));
    assert_eq!(
        r.mock.requests().last().unwrap().body_sha256,
        hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&payload))
    );
    // smuggling-shaped framing refused
    let resp = send(
        fwd,
        format!("POST /v1/completions HTTP/1.1\r\nHost: {host}\r\nContent-Length: 3\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n").as_bytes(),
    )
    .await;
    assert_eq!(resp.status(), 400);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn untrusted_peer_is_disconnected() {
    let r = rig(RigOptions::default()).await;
    let stranger = load_or_create_identity(&r.dir.path().join("stranger")).unwrap();
    let res = start_use(
        stranger,
        UseConfig {
            peer: r.share.dial_addr(),
            bind: "127.0.0.1:0".parse().unwrap(),
            paired: PairedConfig::default(),
            spare_stream: true,
            verbose: false,
        },
    )
    .await;
    if let Ok(handle) = res {
        let host = handle.bind.to_string();
        let resp = send(handle.bind, &request("POST", "/v1/chat/completions", &host, &[], &chat_body("plain"))).await;
        assert_ne!(resp.status(), 200, "stranger got an answer");
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(r.share.stats.lock().unwrap().connections_rejected >= 1);
    assert!(r.mock.requests().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admission_one_running_two_queued_then_503() {
    let r = rig(RigOptions::default()).await;
    let fwd = r.use_side.bind;
    let host = fwd.to_string();
    let req = request("POST", "/v1/chat/completions?n=2&prefill_ms=1200", &host, &[], &chat_body("plain"));
    let mut joins = Vec::new();
    for i in 0..4 {
        let req = req.clone();
        joins.push(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100 * i)).await;
            send(fwd, &req).await
        }));
    }
    let mut statuses = Vec::new();
    for j in joins {
        let resp = j.await.unwrap();
        if resp.status() == 503 {
            assert!(resp.head().contains("Retry-After: 5"), "{}", resp.head());
        }
        statuses.push(resp.status());
    }
    statuses.sort();
    assert_eq!(statuses, vec![200, 200, 200, 503]);
    // light requests are not queued behind inference
    let slow = tokio::spawn({
        let req = req.clone();
        async move { send(fwd, &req).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let t = std::time::Instant::now();
    let health = send(fwd, &request("GET", "/health", &host, &[], b"")).await;
    assert_eq!(health.status(), 200);
    assert!(t.elapsed() < Duration::from_millis(800));
    assert_eq!(slow.await.unwrap().status(), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_rpc_and_headless_model_control() {
    let r = rig(RigOptions::default()).await;
    match r.use_side.status().await.unwrap() {
        StatusReply::Status(s) => {
            assert_eq!(s.name, "Test Studio");
            assert!(s.model_running);
            assert_eq!(s.model_id.as_deref(), Some("slipstream"));
            assert_eq!(s.n_ctx, Some(8192));
            assert!(!s.busy);
            assert!(!s.can_start_model);
            assert_eq!(s.app_version, env!("CARGO_PKG_VERSION"));
        }
        other => panic!("unexpected {other:?}"),
    }
    for op in [StatusRequest::StartModel, StatusRequest::StopModel] {
        match r.use_side.rpc(op).await.unwrap() {
            StatusReply::Refused { code, message } => {
                assert_eq!(code, "not_supported_headless");
                assert!(message.contains("headless"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    // status requests never touch inference: only GETs reached the mock
    assert!(r.mock.requests().iter().all(|s| s.method == "GET"));
}
