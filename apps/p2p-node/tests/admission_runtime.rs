use std::sync::Arc;
use std::time::Duration;

use p2p_core::JobRequest;
use p2p_crypto::NodeKeypair;
use p2p_engine::EngineChoice;
use p2p_node::{
    capability_to_advert, client_hello, default_capability, send_sealed_job, AdmissionController,
    AdmissionRejection, NodeConfig, NodeMode, NodePolicy, RunningNode,
};

fn community_policy() -> NodePolicy {
    let mut policy = NodePolicy::for_mode(NodeMode::Community);
    policy.donate_capacity = true;
    policy
}

#[test]
fn rejects_oversized_tokens_and_frames() {
    let policy = community_policy();
    let max_frame = policy.limits.max_frame_bytes;
    let admission = AdmissionController::new(policy);

    assert_eq!(
        admission.admit("peer-a", 4097, 128).unwrap_err(),
        AdmissionRejection::TokenLimit
    );
    assert_eq!(
        admission.admit("peer-a", 8, 0).unwrap_err(),
        AdmissionRejection::FrameLimit
    );
    assert_eq!(
        admission
            .admit("peer-a", 8, max_frame.saturating_add(1))
            .unwrap_err(),
        AdmissionRejection::FrameLimit
    );
}

#[test]
fn global_concurrency_is_released_with_the_permit() {
    let mut policy = community_policy();
    policy.limits.max_concurrent_jobs = 1;
    let admission = AdmissionController::new(policy);

    let permit = admission.admit("peer-a", 8, 128).unwrap();
    assert_eq!(admission.active_jobs(), 1);
    assert_eq!(
        admission.admit("peer-b", 8, 128).unwrap_err(),
        AdmissionRejection::ConcurrentLimit
    );
    drop(permit);
    assert_eq!(admission.active_jobs(), 0);
    assert!(admission.admit("peer-b", 8, 128).is_ok());
}

#[test]
fn per_peer_rate_does_not_penalize_another_identity() {
    let mut policy = community_policy();
    policy.limits.max_jobs_per_peer_per_window = 2;
    let admission = AdmissionController::new(policy);

    drop(admission.admit("peer-a", 8, 128).unwrap());
    drop(admission.admit("peer-a", 8, 128).unwrap());
    assert_eq!(
        admission.admit("peer-a", 8, 128).unwrap_err(),
        AdmissionRejection::PeerRate
    );
    assert!(admission.admit("peer-b", 8, 128).is_ok());
}

#[test]
fn community_worker_requires_explicit_donation_opt_in() {
    let policy = NodePolicy::for_mode(NodeMode::Community);
    let admission = AdmissionController::new(policy);
    assert_eq!(
        admission.admit("peer-a", 8, 128).unwrap_err(),
        AdmissionRejection::DonationDisabled
    );
}

#[tokio::test]
async fn community_free_job_is_bounded_and_never_faucet_funded() {
    let dir = tempfile::tempdir().unwrap();
    let worker_keypair = NodeKeypair::generate();
    let provider_id = worker_keypair.node_id().as_hex().to_string();
    let provider = worker_keypair.node_id();
    let mut node = RunningNode::open(NodeConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        keypair: Arc::new(worker_keypair),
        capability: default_capability(vec!["mock".into()], true),
        engine: EngineChoice::Mock,
        spawn_engine: false,
        ledger_path: Some(dir.path().join("free.db")),
        bootstrap: vec![],
        policy: community_policy(),
    })
    .unwrap();
    let ledger = node.ledger.clone();
    let (listener, addr) = node.bind().await.unwrap();
    let _server = tokio::spawn(async move {
        let _ = node.accept_loop(listener).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let client = NodeKeypair::generate();
    let consumer_id = client.node_id().as_hex().to_string();
    let advert = capability_to_advert(
        &client.node_id(),
        &default_capability(vec!["mock".into()], true),
        true,
    );
    let (mut session, _) = client_hello(addr, advert, &client, None).await.unwrap();

    let oversized = JobRequest {
        job_id: "free-too-many-tokens".into(),
        model: "mock".into(),
        system: String::new(),
        prompt: "bounded".into(),
        max_tokens: 4097,
    };
    let rejected = send_sealed_job(&mut session, &oversized, &provider, &client)
        .await
        .unwrap();
    assert!(!rejected.ok);
    assert_eq!(rejected.error.as_deref(), Some("token_limit"));

    let valid = JobRequest {
        job_id: "free-valid".into(),
        max_tokens: 2,
        ..oversized
    };
    let completed = send_sealed_job(&mut session, &valid, &provider, &client)
        .await
        .unwrap();
    assert!(completed.ok);
    assert_eq!(ledger.balance(&consumer_id).unwrap(), 0);
    assert_eq!(ledger.balance(&provider_id).unwrap(), 0);
    assert!(ledger.get_settlement("free-valid").unwrap().is_none());
}
