use std::time::Duration;

use futures::StreamExt;
use libp2p::request_response::{Event, Message};
use libp2p::swarm::SwarmEvent;
use p2p_core::{InferenceEngine, JobRequest, MockEngine};
use p2p_crypto::{
    open_job_request, open_job_result, seal_job_request, seal_job_result, NodeKeypair,
    SealedEnvelope,
};
use p2p_libp2p::{build_quic_swarm, MeshEvent};

#[tokio::test]
async fn sealed_inference_roundtrip_over_direct_quic() {
    let requester = NodeKeypair::from_secret_bytes([31; 32]);
    let worker = NodeKeypair::from_secret_bytes([32; 32]);
    let mut client = build_quic_swarm().unwrap();
    let mut server = build_quic_swarm().unwrap();

    server
        .listen_on("/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap())
        .unwrap();
    let server_addr = loop {
        if let SwarmEvent::NewListenAddr { address, .. } = server.select_next_some().await {
            break address;
        }
    };
    client.dial(server_addr).unwrap();

    let request = JobRequest {
        job_id: "quic-sealed-1".into(),
        model: "mock".into(),
        system: String::new(),
        prompt: "direct quic sealed inference".into(),
        max_tokens: 3,
    };
    let sealed = seal_job_request(&request, &worker.node_id()).unwrap();
    let request_bytes = serde_json::to_vec(&sealed).unwrap();
    let mut sent = false;

    let result = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            tokio::select! {
                event = client.select_next_some() => {
                    match event {
                        SwarmEvent::ConnectionEstablished { peer_id, .. } if !sent => {
                            client.behaviour_mut().send_request(&peer_id, request_bytes.clone());
                            sent = true;
                        }
                        SwarmEvent::Behaviour(MeshEvent::Inference(Event::Message { message: Message::Response { response, .. }, .. })) => {
                            let sealed: SealedEnvelope = serde_json::from_slice(&response).unwrap();
                            break open_job_result(&sealed, &requester).unwrap();
                        }
                        _ => {}
                    }
                }
                event = server.select_next_some() => {
                    if let SwarmEvent::Behaviour(MeshEvent::Inference(Event::Message {
                        message: Message::Request { request, channel, .. }, ..
                    })) = event {
                        let sealed: SealedEnvelope = serde_json::from_slice(&request).unwrap();
                        let opened = open_job_request(&sealed, &worker).unwrap();
                        let inferred = MockEngine.infer(&opened);
                        let response = seal_job_result(&inferred, &requester.node_id()).unwrap();
                        server.behaviour_mut()
                            .send_response(channel, serde_json::to_vec(&response).unwrap())
                            .unwrap();
                    }
                }
            }
        }
    }).await.expect("direct QUIC inference timed out");

    assert!(result.ok);
    assert_eq!(result.job_id, "quic-sealed-1");
    assert_eq!(result.tokens, 3);
    assert!(result.text.contains("prompt_hash="));
}
