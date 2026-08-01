//! Integration: two localhost nodes exchange capability + mock encrypted job.

use std::net::{Ipv4Addr, SocketAddr};

use p2p_net::message::{CapabilityAdvert, NetMessage};
use p2p_net::session::{accept, connect, listen};
use p2p_net::BootstrapList;

fn cap(id: &str) -> CapabilityAdvert {
    CapabilityAdvert {
        node_id: id.into(),
        models: vec!["mock-7b".into()],
        ram_gib: 32,
        vram_gib: 0,
        backend: "mock".into(),
    }
}

#[tokio::test]
async fn two_nodes_capability_and_mock_job() {
    let (listener, addr) = listen(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .expect("listen");

    // Bootstrap list points at the listening peer (discovery surface for MVP).
    let bootstrap = BootstrapList::from_addrs([addr]);
    assert_eq!(bootstrap.peers().len(), 1);
    assert_eq!(bootstrap.peers()[0].addr, addr);

    let server = tokio::spawn(async move {
        let mut session = accept(&listener).await.expect("accept");
        let peer_cap = session
            .exchange_hello(cap("node-b"))
            .await
            .expect("server hello");
        assert_eq!(peer_cap.node_id, "node-a");
        assert_eq!(peer_cap.backend, "mock");

        // Expect a mock encrypted job, reply with JobResult.
        match session.recv().await.expect("recv job") {
            NetMessage::EncryptedJob {
                job_id,
                ciphertext,
                ..
            } => {
                assert_eq!(job_id, "job-42");
                assert_eq!(ciphertext, b"mock-sealed-prompt");
                session
                    .send(&NetMessage::JobResult {
                        job_id,
                        ok: true,
                        text: "mock-completion".into(),
                        tokens: 3,
                        error: None,
                    })
                    .await
                    .expect("send result");
            }
            other => panic!("expected EncryptedJob, got {other:?}"),
        }

        // Heartbeat liveness.
        match session.recv().await.expect("recv hb") {
            NetMessage::Heartbeat { seq } => assert_eq!(seq, 1),
            other => panic!("expected Heartbeat, got {other:?}"),
        }
        session
            .send(&NetMessage::Heartbeat { seq: 1 })
            .await
            .expect("hb reply");
    });

    let mut client = connect(addr).await.expect("connect");
    let peer_cap = client
        .exchange_hello(cap("node-a"))
        .await
        .expect("client hello");
    assert_eq!(peer_cap.node_id, "node-b");
    assert!(peer_cap.models.contains(&"mock-7b".into()));

    client
        .send(&NetMessage::EncryptedJob {
            job_id: "job-42".into(),
            ciphertext: b"mock-sealed-prompt".to_vec(),
            nonce: vec![0u8; 12],
            ephemeral_pubkey: vec![0u8; 32],
        })
        .await
        .expect("send job");

    match client.recv().await.expect("recv result") {
        NetMessage::JobResult {
            job_id,
            ok,
            text,
            tokens,
            error,
        } => {
            assert_eq!(job_id, "job-42");
            assert!(ok);
            assert_eq!(text, "mock-completion");
            assert_eq!(tokens, 3);
            assert!(error.is_none());
        }
        other => panic!("expected JobResult, got {other:?}"),
    }

    client
        .send(&NetMessage::Heartbeat { seq: 1 })
        .await
        .expect("hb");
    match client.recv().await.expect("hb ack") {
        NetMessage::Heartbeat { seq } => assert_eq!(seq, 1),
        other => panic!("expected Heartbeat, got {other:?}"),
    }

    server.await.expect("server task");
}
