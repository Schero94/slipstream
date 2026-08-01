//! End-to-end MVP path:
//! discover peer → route → seal → net exchange (with replay) → mock engine → ledger settle.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use p2p_core::{local_capability, BackendKind, JobRequest, NodeId};
use p2p_crypto::NodeKeypair;
use p2p_engine::{open_engine, open_engine_for_choice, select_engine, EngineChoice};
use p2p_ledger::Ledger;
use p2p_node::{
    capability_to_advert, client_hello, default_capability, send_sealed_job, NodeConfig,
    RunningNode,
};
use p2p_router::{choose_route, JobContext, PeerSnapshot, RouteRequest};

#[tokio::test]
async fn sealed_job_mock_infer_and_settle() {
    let dir = tempfile::tempdir().unwrap();
    let ledger_path = dir.path().join("demo.db");
    let worker_key_path = dir.path().join("worker.key");

    let worker_kp = NodeKeypair::generate();
    worker_kp.save(&worker_key_path).unwrap();
    let provider_id = worker_kp.node_id().as_hex().to_string();
    let provider_node_id = worker_kp.node_id();

    let mut node = RunningNode::open(NodeConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        keypair: Arc::new(worker_kp),
        capability: default_capability(vec!["mock".into()], true),
        engine: EngineChoice::Mock,
        spawn_engine: false,
        ledger_path: Some(ledger_path.clone()),
        bootstrap: vec![],
    })
    .unwrap();

    let (listener, addr) = node.bind().await.unwrap();
    let _server = tokio::spawn(async move {
        let _ = node.accept_loop(listener).await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let client_kp = NodeKeypair::generate();
    let consumer_id = client_kp.node_id().as_hex().to_string();
    let cap = default_capability(vec!["mock".into()], true);
    let ours = capability_to_advert(&client_kp.node_id(), &cap, true);
    let (mut session, remote) = client_hello(addr, ours).await.unwrap();
    assert_eq!(remote.node_id, provider_id);
    assert_eq!(remote.backend, "mock");

    // Router selects the discovered worker over an under-gated decoy.
    let job = JobRequest {
        job_id: "job-demo-1".into(),
        model: "mock".into(),
        system: "sys".into(),
        prompt: "hello mesh".into(),
        max_tokens: 4,
    };
    let mut worker_cap = local_capability("linux", 36, 0, vec!["mock".into()]);
    worker_cap.backend = BackendKind::LlamaPgrn;
    worker_cap.tok_s_estimate = 20.0;
    let mut weak_cap = local_capability("linux", 8, 0, vec!["mock".into()]);
    weak_cap.backend = BackendKind::LlamaPgrn;
    weak_cap.tok_s_estimate = 99.0;
    let peers = [
        PeerSnapshot {
            node_id: provider_node_id.clone(),
            listen_addr: addr.to_string(),
            capability: worker_cap,
            rtt_ms: 5,
            reputation: 50,
            load: 0.1,
            is_local: false,
        },
        PeerSnapshot {
            node_id: NodeId::from_bytes(&[9u8; 32]),
            listen_addr: "127.0.0.1:1".into(),
            capability: weak_cap,
            rtt_ms: 1,
            reputation: 100,
            load: 0.0,
            is_local: false,
        },
    ];
    let route = choose_route(
        &peers,
        &RouteRequest::from_job(job.clone(), JobContext::General, None, "linux"),
    )
    .expect("router must pick eligible worker");
    assert_eq!(route.node_id, provider_node_id);
    assert_eq!(route.listen_addr.parse::<SocketAddr>().unwrap(), addr);

    let recipient = NodeId::from_hex(&remote.node_id).unwrap();
    let result = send_sealed_job(&mut session, &job, &recipient, &client_kp)
        .await
        .unwrap();
    assert!(result.ok, "result={result:?}");
    assert_eq!(result.tokens, 4);
    assert!(result.text.contains("tok0"));

    // Settlement: B credited / A debited (faucet 1000 on Hello).
    let ledger = Ledger::open_sqlite(&ledger_path).unwrap();
    let settlement = ledger
        .get_settlement("job-demo-1")
        .unwrap()
        .expect("settled");
    assert_eq!(settlement.provider_id, provider_id);
    assert_eq!(settlement.consumer_id, consumer_id);
    assert_eq!(settlement.tokens, 4);
    assert!(settlement.credits >= 1);
    let provider_bal = ledger.balance(&provider_id).unwrap();
    let consumer_bal = ledger.balance(&consumer_id).unwrap();
    assert_eq!(provider_bal, settlement.credits);
    assert_eq!(consumer_bal, 1000 - settlement.credits);

    // Idempotent re-settle does not double-credit.
    let again = ledger
        .settle("job-demo-1", &consumer_id, &provider_id, 4)
        .unwrap();
    assert!(!again.is_first());
    assert_eq!(ledger.balance(&provider_id).unwrap(), provider_bal);
}

#[tokio::test]
async fn duplicate_job_id_is_rejected_by_recv_with_replay() {
    let dir = tempfile::tempdir().unwrap();
    let worker_kp = NodeKeypair::generate();
    let mut node = RunningNode::open(NodeConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        keypair: Arc::new(worker_kp),
        capability: default_capability(vec!["mock".into()], true),
        engine: EngineChoice::Mock,
        spawn_engine: false,
        ledger_path: Some(dir.path().join("r.db")),
        bootstrap: vec![],
    })
    .unwrap();
    let (listener, addr) = node.bind().await.unwrap();
    let provider = node.config.keypair.node_id();
    let _server = tokio::spawn(async move {
        let _ = node.accept_loop(listener).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let client_kp = NodeKeypair::generate();
    let ours = capability_to_advert(
        &client_kp.node_id(),
        &default_capability(vec!["mock".into()], true),
        true,
    );
    let (mut session, _) = client_hello(addr, ours).await.unwrap();
    let job = JobRequest {
        job_id: "job-replay-1".into(),
        model: "mock".into(),
        system: String::new(),
        prompt: "once".into(),
        max_tokens: 2,
    };
    let first = send_sealed_job(&mut session, &job, &provider, &client_kp)
        .await
        .unwrap();
    assert!(first.ok);

    // Same job_id again → worker nacks via ReplayCache on EncryptedJob.
    let second = send_sealed_job(&mut session, &job, &provider, &client_kp)
        .await
        .unwrap();
    assert!(!second.ok);
    assert_eq!(second.error.as_deref(), Some("replay"));
}

#[test]
fn engine_selection_mac_mlx_else_llama_and_mock_default() {
    let mac = local_capability("macos", 36, 0, vec!["m".into()]);
    let linux = local_capability("linux", 36, 0, vec!["m".into()]);
    assert_eq!(select_engine("macos", &mac), BackendKind::Mlx);
    assert_eq!(select_engine("linux", &linux), BackendKind::LlamaPgrn);

    let mock = open_engine("macos", &mac, true);
    assert!(mock.backend_kind().is_none());
    let mlx = open_engine("macos", &mac, false);
    assert_eq!(mlx.backend_kind(), Some(BackendKind::Mlx));
    let llama = open_engine("linux", &linux, false);
    assert_eq!(llama.backend_kind(), Some(BackendKind::LlamaPgrn));

    // Explicit --engine overrides OS matrix; Auto follows Mac→Mlx else Llama.
    assert_eq!(
        open_engine_for_choice(EngineChoice::Auto, "macos", &mac).backend_kind(),
        Some(BackendKind::Mlx)
    );
    assert_eq!(
        open_engine_for_choice(EngineChoice::Auto, "linux", &linux).backend_kind(),
        Some(BackendKind::LlamaPgrn)
    );
    assert_eq!(
        open_engine_for_choice(EngineChoice::Mlx, "linux", &linux).backend_kind(),
        Some(BackendKind::Mlx)
    );
    assert_eq!(
        open_engine_for_choice(EngineChoice::Llama, "macos", &mac).backend_kind(),
        Some(BackendKind::LlamaPgrn)
    );
}

/// In-process dual-node glue (no TCP): A seals → B opens + mock infer → ledger settle.
#[test]
fn inprocess_seal_infer_settle_debits_a_credits_b() {
    use p2p_core::LocalNode;
    use p2p_crypto::{open_job_request, seal_job_request};

    let a_kp = NodeKeypair::generate();
    let b_kp = NodeKeypair::generate();
    let a_id = a_kp.node_id().as_hex().to_string();
    let b_id = b_kp.node_id().as_hex().to_string();

    let worker = LocalNode::with_mock(
        local_capability("macos", 36, 0, vec!["mock".into()]),
        "inproc://b",
    );

    let job = JobRequest {
        job_id: "job-inproc-1".into(),
        model: "mock".into(),
        system: String::new(),
        prompt: "in-process e2e".into(),
        max_tokens: 3,
    };
    let sealed = seal_job_request(&job, &b_kp.node_id()).unwrap();
    let opened = open_job_request(&sealed, &b_kp).unwrap();
    let result = worker.submit_local(opened);
    assert!(result.ok);
    assert_eq!(result.tokens, 3);

    let ledger = Ledger::open_memory().unwrap();
    ledger.fund(&a_id, 1000).unwrap();
    let outcome = ledger
        .settle(&result.job_id, &a_id, &b_id, u64::from(result.tokens))
        .unwrap();
    assert!(outcome.is_first());
    assert_eq!(ledger.balance(&b_id).unwrap(), outcome.credits());
    assert_eq!(ledger.balance(&a_id).unwrap(), 1000 - outcome.credits());
    let _ = a_kp; // consumer identity used for debit account
}
