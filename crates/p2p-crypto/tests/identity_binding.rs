use p2p_crypto::{verify_identity_signature, NodeKeypair, SigningIdentity};

#[test]
fn signing_identity_is_stable_and_separate_from_encryption_key() {
    let node = NodeKeypair::from_secret_bytes([7; 32]);
    let first = SigningIdentity::from_node_keypair(&node);
    let second = SigningIdentity::from_node_keypair(&node);

    assert_eq!(first.public_bytes(), second.public_bytes());
    assert_ne!(first.public_bytes(), node.public_bytes());
}

#[test]
fn signature_binds_every_payload_byte() {
    let node = NodeKeypair::from_secret_bytes([9; 32]);
    let identity = SigningIdentity::from_node_keypair(&node);
    let payload = b"protocol=1|x25519=abc|models=qwen|expires=42";
    let signature = identity.sign(payload);

    verify_identity_signature(&identity.public_bytes(), payload, &signature).unwrap();
    assert!(verify_identity_signature(
        &identity.public_bytes(),
        b"protocol=1|x25519=attacker|models=qwen|expires=42",
        &signature,
    )
    .is_err());
}

#[test]
fn wrong_identity_and_malformed_signature_fail_closed() {
    let signer = SigningIdentity::from_node_keypair(&NodeKeypair::from_secret_bytes([1; 32]));
    let other = SigningIdentity::from_node_keypair(&NodeKeypair::from_secret_bytes([2; 32]));
    let signature = signer.sign(b"hello");

    assert!(verify_identity_signature(&other.public_bytes(), b"hello", &signature).is_err());
    assert!(verify_identity_signature(&signer.public_bytes(), b"hello", &[0; 12]).is_err());
}
