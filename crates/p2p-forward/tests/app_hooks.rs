//! v0.6 M4 forwarder hooks used by the app: the app window's own origin (CORS, token still
//! required) and the share side's `ModelControl` (StartModel / StopModel through the app).

mod support;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use p2p_forward::{ModelControl, PeerId, ReplyFuture, StatusReply, StatusRequest};
use support::{chat_body, request, rig, send, RigOptions};

const TOKEN: &str = "4f1c0e0b9a7d6c5b4a39281706f5e4d3c2b1a09f8e7d6c5b4a3928170605f4e3";
const APP: &str = "tauri://localhost";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn app_origin_gets_cors_but_still_needs_the_token() {
    let r = rig(RigOptions {
        token: Some(TOKEN.into()),
        app_origin: Some(APP.into()),
        ..RigOptions::default()
    })
    .await;
    let fwd = r.use_side.bind;
    let host = fwd.to_string();
    let good = format!("Bearer {TOKEN}");

    // Preflight from the app window: answered locally, nothing reaches the big Mac.
    let pre = send(
        fwd,
        &request(
            "OPTIONS",
            "/v1/chat/completions",
            &host,
            &[("Origin", APP), ("Access-Control-Request-Method", "POST"), ("Access-Control-Request-Headers", "authorization, content-type")],
            b"",
        ),
    )
    .await;
    assert_eq!(pre.status(), 204, "{}", pre.head());
    assert!(pre.head().contains(&format!("Access-Control-Allow-Origin: {APP}")));
    assert!(pre.head().to_ascii_lowercase().contains("access-control-allow-headers: authorization, content-type"));

    // App origin without the token: 401, with CORS so the window can read the reason.
    let no_tok = send(fwd, &request("POST", "/v1/chat/completions", &host, &[("Origin", APP)], &chat_body("plain"))).await;
    assert_eq!(no_tok.status(), 401);
    assert!(no_tok.head().contains(&format!("Access-Control-Allow-Origin: {APP}")));

    // App origin + token: forwarded, response carries exactly one ACAO for the app.
    let ok = send(
        fwd,
        &request(
            "POST",
            "/v1/chat/completions",
            &host,
            &[("Origin", APP), ("Sec-Fetch-Site", "cross-site"), ("Authorization", &good)],
            &chat_body("plain"),
        ),
    )
    .await;
    assert_eq!(ok.status(), 200, "{}", ok.head());
    assert_eq!(ok.head().matches("Access-Control-Allow-Origin").count(), 1, "{}", ok.head());
    let seen = r.mock.requests();
    assert_eq!(seen.len(), 1);
    assert!(!seen[0].headers.iter().any(|(k, _)| k == "origin" || k == "authorization"));

    // Any other origin (or two Origin headers, or a preflight from a web page) is refused.
    for extra in [
        vec![("Origin", "http://evil.example"), ("Authorization", good.as_str())],
        vec![("Origin", "null"), ("Authorization", good.as_str())],
        vec![("Origin", APP), ("Origin", "http://evil.example"), ("Authorization", good.as_str())],
    ] {
        let resp = send(fwd, &request("POST", "/v1/chat/completions", &host, &extra, &chat_body("plain"))).await;
        assert_eq!(resp.status(), 403, "{extra:?}: {}", resp.head());
        assert!(!resp.head().contains("Access-Control-Allow-Origin"));
    }
    let evil_pre = send(fwd, &request("OPTIONS", "/v1/chat/completions", &host, &[("Origin", "http://evil.example")], b"")).await;
    assert_eq!(evil_pre.status(), 403);
    assert_eq!(r.mock.requests().len(), 1, "refusals never reach the big Mac");
    // Editors (no Origin) are unchanged.
    let editor = send(fwd, &request("GET", "/v1/models", &host, &[("Authorization", &good)], b"")).await;
    assert_eq!(editor.status(), 200);
    assert!(!editor.head().contains("Access-Control-Allow-Origin"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn app_origin_without_token_is_refused_at_start() {
    let dir = tempfile::tempdir().unwrap();
    let node = p2p_forward::load_or_create_node(dir.path()).unwrap();
    let other = p2p_forward::load_or_create_node(&dir.path().join("o")).unwrap();
    let other_id = p2p_forward::transport_identity(&other).public().to_peer_id();
    let err = p2p_forward::start_use(
        p2p_forward::transport_identity(&node),
        p2p_forward::UseConfig {
            peer: format!("/ip4/127.0.0.1/udp/9/quic-v1/p2p/{other_id}").parse().unwrap(),
            trust: p2p_forward::StaticTrust::new([(other_id, other.public_bytes())]),
            node,
            bind: "127.0.0.1:0".parse().unwrap(),
            paired: Default::default(),
            spare_stream: false,
            token: None,
            local: Default::default(),
            discovery: None,
            app_origin: Some(APP.into()),
            persistent: false,
            connect_wait: p2p_forward::client::CONNECT_WAIT,
            versions: Default::default(),
            before_dial: None,
            verbose: false,
        },
    )
    .await
    .err()
    .expect("refused");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}

struct TestModel {
    allowed: bool,
    starts: AtomicU32,
    stops: AtomicU32,
}

impl ModelControl for TestModel {
    fn allowed(&self, _peer: &PeerId) -> bool {
        self.allowed
    }
    fn start(&self, _peer: PeerId) -> ReplyFuture {
        self.starts.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { StatusReply::Status(p2p_forward::Status { name: "started".into(), ..Default::default() }) })
    }
    fn stop(&self, _peer: PeerId) -> ReplyFuture {
        self.stops.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            StatusReply::Refused {
                code: "local_request".into(),
                message: "busy with its own work".into(),
            }
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn start_and_stop_model_go_through_model_control() {
    for allowed in [true, false] {
        let model = Arc::new(TestModel {
            allowed,
            starts: AtomicU32::new(0),
            stops: AtomicU32::new(0),
        });
        let r = rig(RigOptions {
            model: Some(model.clone()),
            ..RigOptions::default()
        })
        .await;
        match r.use_side.status().await.unwrap() {
            StatusReply::Status(s) => assert_eq!(s.can_start_model, allowed),
            other => panic!("{other:?}"),
        }
        let start = r.use_side.rpc(StatusRequest::StartModel).await.unwrap();
        let stop = r.use_side.rpc(StatusRequest::StopModel).await.unwrap();
        if allowed {
            assert!(matches!(start, StatusReply::Status(ref s) if s.name == "started"), "{start:?}");
            assert!(matches!(stop, StatusReply::Refused { ref code, .. } if code == "local_request"), "{stop:?}");
            assert_eq!((model.starts.load(Ordering::SeqCst), model.stops.load(Ordering::SeqCst)), (1, 1));
        } else {
            for reply in [start, stop] {
                assert!(matches!(reply, StatusReply::Refused { ref code, .. } if code == "model_control_off"), "{reply:?}");
            }
            assert_eq!(model.starts.load(Ordering::SeqCst), 0);
        }
    }
    // Headless share (no ModelControl): refused as before, can_start_model false.
    let r = rig(RigOptions::default()).await;
    match r.use_side.rpc(StatusRequest::StartModel).await.unwrap() {
        StatusReply::Refused { code, .. } => assert_eq!(code, "not_supported_headless"),
        other => panic!("{other:?}"),
    }
}
