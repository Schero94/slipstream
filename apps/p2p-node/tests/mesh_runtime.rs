use std::sync::Arc;

use p2p_core::JobRequest;
use p2p_crypto::{open_job_result, seal_job_request, NodeKeypair};
use p2p_net::NetMessage;
use p2p_node::mesh::{
    transport_peer_id, validate_mesh_listen, MeshRequest, MeshResponse, MeshWorker,
};
use p2p_node::{
    capability_to_advert, default_capability, hello_nonce, net_to_sealed, sealed_to_net,
    signed_hello, verify_signed_hello, EngineChoice, NodeConfig, NodeMode, NodePolicy, RunningNode,
};

fn open_worker(key: Arc<NodeKeypair>) -> RunningNode {
    RunningNode::open(NodeConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        keypair: key,
        capability: default_capability(vec!["mock".into()], true),
        engine: EngineChoice::Mock,
        spawn_engine: false,
        ledger_path: None,
        bootstrap: vec![],
        policy: NodePolicy::for_mode(NodeMode::Local),
    })
    .unwrap()
}

#[test]
fn persistent_transport_peer_id_is_stable_and_key_specific() {
    let a1 = NodeKeypair::from_secret_bytes([71; 32]);
    let a2 = NodeKeypair::from_secret_bytes([71; 32]);
    let b = NodeKeypair::from_secret_bytes([72; 32]);
    assert_eq!(
        transport_peer_id(&a1).unwrap(),
        transport_peer_id(&a2).unwrap()
    );
    assert_ne!(
        transport_peer_id(&a1).unwrap(),
        transport_peer_id(&b).unwrap()
    );
}

#[test]
fn local_mesh_mode_refuses_non_loopback_listen() {
    assert!(validate_mesh_listen(
        NodeMode::Local,
        &"/ip4/127.0.0.1/udp/9003/quic-v1".parse().unwrap()
    )
    .is_ok());
    assert!(validate_mesh_listen(
        NodeMode::Local,
        &"/ip4/0.0.0.0/udp/9003/quic-v1".parse().unwrap()
    )
    .is_err());
    assert!(validate_mesh_listen(
        NodeMode::Community,
        &"/ip4/0.0.0.0/udp/9003/quic-v1".parse().unwrap()
    )
    .is_ok());
}

#[test]
fn signed_hello_must_match_the_authenticated_quic_peer() {
    let worker_key = Arc::new(NodeKeypair::from_secret_bytes([75; 32]));
    let signed_client = NodeKeypair::from_secret_bytes([76; 32]);
    let different_transport = NodeKeypair::from_secret_bytes([77; 32]);
    let worker = MeshWorker::new(open_worker(worker_key));
    let advert = capability_to_advert(
        &signed_client.node_id(),
        &default_capability(vec!["mock".into()], true),
        true,
    );
    let hello = signed_hello(advert, &signed_client, Vec::new()).unwrap();
    let probe = serde_json::to_vec(&MeshRequest::Probe { hello }).unwrap();
    let response: MeshResponse = serde_json::from_slice(
        &worker.handle_request(transport_peer_id(&different_transport).unwrap(), &probe),
    )
    .unwrap();
    assert!(matches!(response, MeshResponse::Error { .. }));
}

#[test]
fn worker_adapter_requires_challenge_and_returns_only_sealed_result() {
    let worker_key = Arc::new(NodeKeypair::from_secret_bytes([73; 32]));
    let requester = NodeKeypair::from_secret_bytes([74; 32]);
    let peer = transport_peer_id(&requester).unwrap();
    let worker = MeshWorker::new(open_worker(Arc::clone(&worker_key)));
    let requester_advert = capability_to_advert(
        &requester.node_id(),
        &default_capability(vec!["mock".into()], true),
        true,
    );

    let probe_hello = signed_hello(requester_advert.clone(), &requester, Vec::new()).unwrap();
    let probe_challenge = hello_nonce(&probe_hello).unwrap();
    let probe = serde_json::to_vec(&MeshRequest::Probe { hello: probe_hello }).unwrap();
    let response: MeshResponse =
        serde_json::from_slice(&worker.handle_request(peer, &probe)).unwrap();
    let MeshResponse::Hello { hello } = response else {
        panic!("expected authenticated worker Hello")
    };
    let worker_advert = verify_signed_hello(&hello, Some(&probe_challenge), None).unwrap();
    assert_eq!(worker_advert.node_id, worker_key.node_id().as_hex());
    let worker_challenge = hello_nonce(&hello).unwrap();

    let request = JobRequest {
        job_id: "mesh-adapter-1".into(),
        model: "mock".into(),
        system: String::new(),
        prompt: "sealed adapter test".into(),
        max_tokens: 2,
    };
    let sealed = seal_job_request(&request, &worker_key.node_id()).unwrap();
    let encrypted_job = sealed_to_net(&request.job_id, &sealed).unwrap();
    let client_hello = signed_hello(requester_advert, &requester, worker_challenge).unwrap();
    let infer = serde_json::to_vec(&MeshRequest::Infer {
        hello: client_hello,
        encrypted_job,
    })
    .unwrap();
    let response: MeshResponse =
        serde_json::from_slice(&worker.handle_request(peer, &infer)).unwrap();
    let MeshResponse::Result { encrypted_result } = response else {
        panic!("expected sealed inference result")
    };
    let NetMessage::EncryptedJobResult {
        job_id,
        ciphertext,
        ephemeral_pubkey,
        ..
    } = encrypted_result
    else {
        panic!("cleartext downgrade")
    };
    let envelope = net_to_sealed(&ciphertext, &ephemeral_pubkey).unwrap();
    let result = open_job_result(&envelope, &requester).unwrap();
    assert_eq!(job_id, request.job_id);
    assert_eq!(result.job_id, request.job_id);
    assert_eq!(result.tokens, 2);

    // The worker challenge is one-shot: replay is rejected before inference.
    let replay: MeshResponse =
        serde_json::from_slice(&worker.handle_request(peer, &infer)).unwrap();
    assert!(matches!(replay, MeshResponse::Error { .. }));
}
