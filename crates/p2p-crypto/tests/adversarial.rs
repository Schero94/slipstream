//! Adversarial / negative tests for transit crypto (security lane).
//!
//! Does not modify `p2p-crypto` library code. Complements
//! `docs/p2p/THREAT_MODEL.md` TM-001 (worker sees plaintext after open) and
//! integrity properties of AEAD (tamper / wrong recipient).

use p2p_core::JobRequest;
use p2p_crypto::{
    open, open_job, open_job_request, seal, seal_job, seal_job_request, CryptoError, NodeKeypair,
    SealedEnvelope,
};

fn secret_job() -> JobRequest {
    JobRequest {
        job_id: "adv-1".into(),
        model: "mock".into(),
        system: "do not leak".into(),
        prompt: "API_KEY=super-secret".into(),
        max_tokens: 16,
    }
}

/// On-path eavesdropper without recipient key cannot open.
#[test]
fn adversary_without_recipient_key_cannot_decrypt() {
    let bob = NodeKeypair::generate();
    let eve = NodeKeypair::generate();
    let sealed = seal(b"API_KEY=super-secret", &bob.public_key()).unwrap();
    assert_eq!(open(&sealed, &eve).unwrap_err(), CryptoError::Decrypt);
}

/// Bit-flip / truncations of ciphertext must fail closed.
#[test]
fn ciphertext_bitflip_and_truncation_fail_closed() {
    let bob = NodeKeypair::generate();
    let sealed = seal(b"payload", &bob.public_key()).unwrap();

    let mut flipped = sealed.clone();
    let mut ct = hex::decode(&flipped.ciphertext_hex).unwrap();
    let last = ct.len() - 1;
    ct[last] ^= 0x01;
    flipped.ciphertext_hex = hex::encode(ct);
    assert_eq!(open(&flipped, &bob).unwrap_err(), CryptoError::Decrypt);

    let mut trunc = sealed.clone();
    let ct = hex::decode(&trunc.ciphertext_hex).unwrap();
    trunc.ciphertext_hex = hex::encode(&ct[..ct.len().saturating_sub(2)]);
    assert_eq!(open(&trunc, &bob).unwrap_err(), CryptoError::Decrypt);
}

/// Swap ephemeral pub with attacker key → decrypt must fail (not confuse KDF).
#[test]
fn swapped_ephemeral_public_key_fails() {
    let bob = NodeKeypair::generate();
    let attacker = NodeKeypair::generate();
    let mut sealed = seal(b"prompt", &bob.public_key()).unwrap();
    sealed.eph_pub_hex = attacker.public_hex();
    assert_eq!(open(&sealed, &bob).unwrap_err(), CryptoError::Decrypt);
}

/// Malformed key material rejected (not panics).
#[test]
fn malformed_envelope_fields_rejected() {
    let bob = NodeKeypair::generate();
    let bad = SealedEnvelope {
        eph_pub_hex: "deadbeef".into(),
        ciphertext_hex: "00".into(),
    };
    assert!(matches!(
        open(&bad, &bob).unwrap_err(),
        CryptoError::BadKey | CryptoError::Decrypt
    ));
}

/// Honest recipient (worker) **can** read prompt — documents TM-001 limit.
/// This is not a bug; it is the confidentiality boundary.
#[test]
fn intended_worker_learns_plaintext_after_open() {
    let worker = NodeKeypair::generate();
    let job = secret_job();
    let sealed = seal_job_request(&job, &worker.node_id()).unwrap();
    let opened = open_job_request(&sealed, &worker).unwrap();
    assert!(opened.prompt.contains("API_KEY"));
}

/// Cross-node job open: only `to` can open payload.
#[test]
fn sealed_job_only_recipient_opens() {
    let alice = NodeKeypair::generate();
    let bob = NodeKeypair::generate();
    let charlie = NodeKeypair::generate();
    let job = secret_job();
    let sealed = seal_job(&job, &alice.node_id(), &bob.node_id()).unwrap();
    assert_eq!(open_job(&sealed, &bob).unwrap().prompt, job.prompt);
    assert!(open_job(&sealed, &alice).is_err());
    assert!(open_job(&sealed, &charlie).is_err());
}

/// Two seals of same plaintext produce distinct envelopes (ephemeral freshness).
#[test]
fn repeated_seal_not_bitwise_identical() {
    let bob = NodeKeypair::generate();
    let a = seal(b"same", &bob.public_key()).unwrap();
    let b = seal(b"same", &bob.public_key()).unwrap();
    assert_ne!(a.eph_pub_hex, b.eph_pub_hex);
    assert_ne!(a.ciphertext_hex, b.ciphertext_hex);
    assert_eq!(open(&a, &bob).unwrap(), b"same");
    assert_eq!(open(&b, &bob).unwrap(), b"same");
}
