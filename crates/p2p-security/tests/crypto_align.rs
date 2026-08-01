//! Align security contracts with the green `p2p-crypto` seal/open API.
//!
//! Does not modify `p2p-crypto`. Asserts composition:
//! - wrong recipient key → `CryptoError::Decrypt`
//! - ciphertext tamper → `CryptoError::Decrypt`
//! - valid envelope replay → rejected by `ReplayCache` (AEAD alone is insufficient)

use std::time::Duration;

use p2p_core::JobRequest;
use p2p_crypto::{open, open_job_request, seal, seal_job_request, CryptoError, NodeKeypair};
use p2p_security::{
    envelope_fingerprint, AttestationStatus, PromptPolicy, PromptSensitivity, ReplayCache,
    ReplayError, RouteTarget, TeeRequirement,
};

fn sample_job(job_id: &str) -> JobRequest {
    JobRequest {
        job_id: job_id.into(),
        model: "mock".into(),
        system: "sys".into(),
        prompt: "align-secret".into(),
        max_tokens: 8,
    }
}

/// p2p-crypto: wrong static key cannot open (transit confidentiality vs non-recipient).
#[test]
fn crypto_wrong_key_fails_open() {
    let bob = NodeKeypair::generate();
    let eve = NodeKeypair::generate();
    let sealed = seal(b"align-secret", &bob.public_key()).unwrap();
    assert_eq!(open(&sealed, &eve).unwrap_err(), CryptoError::Decrypt);
    assert_eq!(open(&sealed, &bob).unwrap(), b"align-secret");
}

/// p2p-crypto: bit-flip tamper fails closed.
#[test]
fn crypto_tamper_fails_open() {
    let bob = NodeKeypair::generate();
    let mut sealed = seal(b"align-secret", &bob.public_key()).unwrap();
    let mut ct = hex::decode(&sealed.ciphertext_hex).unwrap();
    ct[0] ^= 0xff;
    sealed.ciphertext_hex = hex::encode(ct);
    assert_eq!(open(&sealed, &bob).unwrap_err(), CryptoError::Decrypt);
}

/// Capture a valid sealed job and replay `job_id` + envelope fingerprint → reject.
#[test]
fn crypto_valid_envelope_replay_rejected_by_cache() {
    let worker = NodeKeypair::generate();
    let job = sample_job("job-replay-1");
    let sealed = seal_job_request(&job, &worker.node_id()).unwrap();
    // AEAD still opens — replay is a protocol/policy problem.
    assert_eq!(open_job_request(&sealed, &worker).unwrap().prompt, job.prompt);

    let fp = envelope_fingerprint(&sealed.eph_pub_hex, &sealed.ciphertext_hex);
    let mut cache = ReplayCache::new(Duration::from_secs(3600));
    cache.accept(&job.job_id, Some(&fp)).unwrap();
    let attack = cache.accept(&job.job_id, Some(&fp));
    assert_eq!(attack, Err(ReplayError::DuplicateJobId));
}

/// Fresh seal of same plaintext gets a new fingerprint (ephemeral); same job_id still blocked.
#[test]
fn crypto_reseal_same_job_id_still_replay() {
    let worker = NodeKeypair::generate();
    let job = sample_job("job-replay-2");
    let a = seal_job_request(&job, &worker.node_id()).unwrap();
    let b = seal_job_request(&job, &worker.node_id()).unwrap();
    let fp_a = envelope_fingerprint(&a.eph_pub_hex, &a.ciphertext_hex);
    let fp_b = envelope_fingerprint(&b.eph_pub_hex, &b.ciphertext_hex);
    assert_ne!(fp_a, fp_b);

    let mut cache = ReplayCache::new(Duration::from_secs(3600));
    cache.accept(&job.job_id, Some(&fp_a)).unwrap();
    // Different envelope bytes, same job_id → still replay.
    assert_eq!(
        cache.accept(&job.job_id, Some(&fp_b)),
        Err(ReplayError::DuplicateJobId)
    );
}

/// TM-001 composition: crypto open succeeds for worker, but Secret remote route blocked.
#[test]
fn crypto_open_ok_does_not_authorize_secret_remote() {
    let worker = NodeKeypair::generate();
    let job = sample_job("job-secret");
    let sealed = seal_job_request(&job, &worker.node_id()).unwrap();
    assert!(open_job_request(&sealed, &worker).is_ok());

    let policy = PromptPolicy::default();
    assert!(policy
        .allow_route(
            PromptSensitivity::Secret,
            RouteTarget::Remote,
            AttestationStatus::None,
            TeeRequirement::NotRequired,
        )
        .is_err());
}
