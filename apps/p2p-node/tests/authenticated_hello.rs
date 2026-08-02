use p2p_crypto::{NodeKeypair, SigningIdentity};
use p2p_net::{CapabilityAdvert, NetMessage};
use p2p_node::{signed_hello_at, verify_signed_hello_at};

fn capability(node: &NodeKeypair) -> CapabilityAdvert {
    CapabilityAdvert {
        node_id: node.node_id().as_hex().to_string(),
        identity_id: String::new(),
        models: vec!["mock".into()],
        ram_gib: 32,
        vram_gib: 0,
        backend: "mock".into(),
    }
}

#[test]
fn signed_hello_binds_capability_encryption_key_and_challenge() {
    let node = NodeKeypair::from_secret_bytes([11; 32]);
    let response_to = vec![7; 32];
    let message = signed_hello_at(
        capability(&node),
        &node,
        response_to.clone(),
        1_000,
        [9; 32],
    )
    .unwrap();
    let identity_id = SigningIdentity::from_node_keypair(&node).public_hex();

    let verified =
        verify_signed_hello_at(&message, Some(&response_to), Some(&identity_id), 1_001).unwrap();
    assert_eq!(verified.node_id, node.node_id().as_hex());
    assert_eq!(verified.identity_id, identity_id);

    let mut tampered = message.clone();
    let NetMessage::Hello { capability, .. } = &mut tampered else {
        unreachable!()
    };
    capability.models.push("attacker-model".into());
    assert!(verify_signed_hello_at(&tampered, Some(&response_to), None, 1_001).is_err());
}

#[test]
fn expired_wrong_challenge_and_wrong_expected_identity_fail_closed() {
    let node = NodeKeypair::from_secret_bytes([12; 32]);
    let message = signed_hello_at(capability(&node), &node, vec![1; 32], 2_000, [2; 32]).unwrap();

    assert!(verify_signed_hello_at(&message, Some(&[9; 32]), None, 2_001).is_err());
    assert!(verify_signed_hello_at(&message, Some(&[1; 32]), Some("wrong"), 2_001).is_err());
    assert!(verify_signed_hello_at(&message, Some(&[1; 32]), None, 2_301).is_err());
}

#[test]
fn unsigned_legacy_hello_is_rejected_by_product_verifier() {
    let node = NodeKeypair::from_secret_bytes([13; 32]);
    let legacy = NetMessage::Hello {
        capability: capability(&node),
        auth: None,
    };
    assert!(verify_signed_hello_at(&legacy, None, None, 1_000).is_err());
}
