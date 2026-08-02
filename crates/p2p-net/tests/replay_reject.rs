//! Duplicate sealed EncryptedJob frames are rejected when ReplayCache is used.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use p2p_net::message::{CapabilityAdvert, NetMessage};
use p2p_net::session::{accept, connect, listen};
use p2p_net::NetError;
use p2p_security::ReplayCache;

fn cap(id: &str) -> CapabilityAdvert {
    CapabilityAdvert {
        node_id: id.into(),
        identity_id: String::new(),
        models: vec!["mock-7b".into()],
        ram_gib: 32,
        vram_gib: 0,
        backend: "mock".into(),
    }
}

fn mock_job() -> NetMessage {
    NetMessage::EncryptedJob {
        job_id: "job-replay".into(),
        ciphertext: b"mock-sealed-prompt".to_vec(),
        nonce: vec![0u8; 12],
        ephemeral_pubkey: vec![0xAB; 32],
    }
}

#[tokio::test]
async fn duplicate_encrypted_job_rejected_with_replay_cache() {
    let (listener, addr) = listen(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .expect("listen");

    let server = tokio::spawn(async move {
        let mut session = accept(&listener).await.expect("accept");
        let _ = session
            .exchange_hello(cap("node-b"))
            .await
            .expect("server hello");

        let mut cache = ReplayCache::new(Duration::from_secs(3600));

        let first = session
            .recv_with_replay(&mut cache)
            .await
            .expect("first job admitted");
        match &first {
            NetMessage::EncryptedJob { job_id, .. } => assert_eq!(job_id, "job-replay"),
            other => panic!("expected EncryptedJob, got {other:?}"),
        }

        let replay_err = session
            .recv_with_replay(&mut cache)
            .await
            .expect_err("duplicate must fail");
        assert!(
            replay_err.is_replay(),
            "expected NetError::Replay, got {replay_err:?}"
        );
        match &replay_err {
            NetError::Replay(msg) => {
                assert!(
                    msg.contains("job-replay"),
                    "replay error should name job_id, got {msg}"
                );
            }
            other => panic!("expected NetError::Replay, got {other:?}"),
        }

        // Ack so client can finish cleanly.
        session
            .send(&NetMessage::JobResult {
                job_id: "job-replay".into(),
                ok: false,
                text: String::new(),
                tokens: 0,
                error: Some("replay".into()),
            })
            .await
            .expect("send nack");
    });

    let mut client = connect(addr).await.expect("connect");
    let _ = client
        .exchange_hello(cap("node-a"))
        .await
        .expect("client hello");

    let job = mock_job();
    client.send(&job).await.expect("send first");
    // Identical sealed envelope (same job_id + ciphertext + eph key).
    client.send(&job).await.expect("send duplicate");

    let _ = client.recv().await.expect("recv nack");
    server.await.expect("server task");
}

#[test]
fn admit_helper_rejects_duplicate_without_tcp() {
    use p2p_net::admit_encrypted_job;

    let mut cache = ReplayCache::new(Duration::from_secs(60));
    let job = mock_job();
    admit_encrypted_job(&job, &mut cache).unwrap();
    let err = admit_encrypted_job(&job, &mut cache).unwrap_err();
    assert!(err.is_replay());
}
