//! Documentation-driven attack scenarios from `docs/p2p/THREAT_MODEL.md`.
//!
//! Each test names an attack, applies the MVP mitigation, and asserts the
//! attack fails (mitigation holds). These are security contracts — not
//! exploits against live systems.

use std::time::Duration;

use p2p_security::{
    AdmissionDecision, Allowlist, AttestationStatus, CompletionReceipt, DosLimits, ModelDigest,
    ModelPinError, ModelRegistry, PromptPolicy, PromptSensitivity, ReplayCache, ReplayError,
    RouteTarget, SettlementError, SettlementGuard, SybilError, TeeRequirement,
};

/// TM-003 — Sybil: unknown identity tries to advertise/earn.
#[test]
fn sybil_unknown_peer_rejected() {
    let allow = Allowlist::new(["alice_pk", "bob_pk"]);
    assert!(allow.admit_advertiser("alice_pk").is_ok());
    // Attack: spawn fresh identity not on bootstrap list.
    let attack = allow.admit_advertiser("sybil_0001");
    assert_eq!(attack, Err(SybilError::NotAllowlisted));
}

/// TM-004 — Replay: same job_id submitted twice.
#[test]
fn replayed_job_id_rejected() {
    let mut cache = ReplayCache::new(Duration::from_secs(3600));
    cache.accept("job-42", Some("env-fp-1")).unwrap();
    // Attack: replay captured JobRequest with same job_id.
    let attack = cache.accept("job-42", Some("env-fp-1"));
    assert_eq!(attack, Err(ReplayError::DuplicateJobId));
    assert!(cache.contains("job-42"));
}

/// TM-005 — Underpay: consumer offers less than quoted credits.
#[test]
fn underpay_rejected() {
    let mut guard = SettlementGuard::new();
    let quoted = SettlementGuard::quote(1500, 2); // 2 units * 2 = 4
    guard.record_completion(CompletionReceipt {
        job_id: "job-u".into(),
        worker_id_hex: "bob".into(),
        tokens: 1500,
        quoted_credits: quoted,
    });
    // Attack: pay 1 when quote is 4.
    let attack = guard.settle("job-u", 1);
    assert_eq!(
        attack,
        Err(SettlementError::Underpay {
            offered: 1,
            quoted: 4
        })
    );
}

/// TM-005 — Freeload: credit without completion receipt.
#[test]
fn freeload_without_receipt_rejected() {
    let mut guard = SettlementGuard::new();
    // Attack: CreditNotice with no prior completion.
    let attack = guard.settle("never-ran", 10);
    assert_eq!(attack, Err(SettlementError::MissingReceipt));
}

/// TM-005 — Honest path still settles.
#[test]
fn honest_settlement_accepted() {
    let mut guard = SettlementGuard::new();
    let quoted = SettlementGuard::quote(100, 1);
    guard.record_completion(CompletionReceipt {
        job_id: "job-ok".into(),
        worker_id_hex: "bob".into(),
        tokens: 100,
        quoted_credits: quoted,
    });
    assert_eq!(guard.settle("job-ok", quoted).unwrap(), quoted);
    assert!(guard.is_settled("job-ok"));
    assert_eq!(
        guard.settle("job-ok", quoted),
        Err(SettlementError::AlreadySettled)
    );
}

/// TM-001 / TM-008 — Secret prompt to unattested remote = exfil assumption blocked.
#[test]
fn secret_prompt_blocked_on_unattested_remote() {
    let policy = PromptPolicy::default();
    // Attack assumption: transit crypto somehow makes remote "safe". It does not.
    let attack = policy.allow_route(
        PromptSensitivity::Secret,
        RouteTarget::Remote,
        AttestationStatus::None,
        TeeRequirement::NotRequired,
    );
    assert!(attack.is_err());
}

/// TM-001 — Unverified TEE claim must not unlock Secret remote.
#[test]
fn fake_tee_claim_does_not_unlock_secret() {
    let policy = PromptPolicy::default();
    let fake = AttestationStatus::from_peer_claim(true, false, true);
    assert_eq!(fake, AttestationStatus::UnverifiedClaim);
    let attack = policy.allow_route(
        PromptSensitivity::Secret,
        RouteTarget::Remote,
        fake,
        TeeRequirement::Required,
    );
    assert!(attack.is_err());
}

/// TM-008 — TEE optional: Public + Attestation::None is valid.
#[test]
fn tee_optional_none_is_valid_for_public() {
    let policy = PromptPolicy::default();
    assert!(policy
        .allow_route(
            PromptSensitivity::Public,
            RouteTarget::Remote,
            AttestationStatus::None,
            TeeRequirement::Optional,
        )
        .is_ok());
}

/// Later path: verified TEE unlocks Secret remote when required.
#[test]
fn verified_tee_allows_secret_remote() {
    let policy = PromptPolicy::default();
    let ok = AttestationStatus::Verified {
        measurement_ok: true,
    };
    assert!(policy
        .allow_route(
            PromptSensitivity::Secret,
            RouteTarget::Remote,
            ok,
            TeeRequirement::Required,
        )
        .is_ok());
}

/// Local route always ok even for Secret (worker is self).
#[test]
fn secret_local_always_allowed() {
    let policy = PromptPolicy::default();
    assert!(policy
        .allow_route(
            PromptSensitivity::Secret,
            RouteTarget::Local,
            AttestationStatus::None,
            TeeRequirement::NotRequired,
        )
        .is_ok());
}

/// TM-002 — Malicious / substituted model weights (digest mismatch).
#[test]
fn tampered_model_digest_rejected() {
    let mut reg = ModelRegistry::new();
    let honest = ModelDigest::from_bytes(b"honest-weights-v1");
    reg.pin("mock-v1", honest.clone());
    // Attack: swap bytes on disk / serve alternate artifact.
    let malice = ModelDigest::from_bytes(b"backdoored-weights");
    assert_ne!(honest.as_hex(), malice.as_hex());
    assert_eq!(
        reg.verify("mock-v1", &malice),
        Err(ModelPinError::DigestMismatch)
    );
}

#[test]
fn unknown_model_not_pinned() {
    let reg = ModelRegistry::new();
    let d = ModelDigest::from_bytes(b"x");
    assert_eq!(reg.verify("nope", &d), Err(ModelPinError::UnknownModel));
}

/// TM-006 — DoS: exceed concurrent / token / peer rate / frame limits.
#[test]
fn dos_over_limit_rejected() {
    let limits = DosLimits {
        max_concurrent_jobs: 2,
        max_tokens_per_job: 128,
        max_jobs_per_peer_per_window: 3,
        max_frame_bytes: 1024,
    };
    assert_eq!(
        limits.admit(2, 0, 16, 100),
        AdmissionDecision::RejectConcurrentLimit
    );
    assert_eq!(
        limits.admit(0, 0, 10_000, 100),
        AdmissionDecision::RejectTokenLimit
    );
    assert_eq!(
        limits.admit(0, 3, 16, 100),
        AdmissionDecision::RejectPeerRate
    );
    assert_eq!(
        limits.admit(0, 0, 16, 4096),
        AdmissionDecision::RejectFrameLimit
    );
    assert_eq!(limits.admit(1, 1, 16, 100), AdmissionDecision::Allow);
}

/// Sensitive remote blocked by default (prompt exfil assumption).
#[test]
fn sensitive_remote_blocked_without_override() {
    let policy = PromptPolicy::default();
    assert!(policy
        .allow_route(
            PromptSensitivity::Sensitive,
            RouteTarget::Remote,
            AttestationStatus::None,
            TeeRequirement::Optional,
        )
        .is_err());
}
