//! Attestation / TEE optional path (TM-008).
//!
//! MVP default is `AttestationStatus::None`. TEE is never implied. Secret
//! prompts may require `Verified` only when quote verification is wired later.

/// Whether the operator/policy demands a TEE for this job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeeRequirement {
    /// TEE must not be required (Public / local).
    NotRequired,
    /// Job may use TEE if available.
    Optional,
    /// Secret remote jobs: must have verified attestation (post-MVP).
    Required,
    /// Explicitly forbid claiming TEE (tests / hostile ads).
    Forbidden,
}

/// Result of (future) remote attestation verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttestationStatus {
    /// No attestation presented — **MVP default**.
    None,
    /// Peer claimed TEE but quote was not verified (treat as hostile).
    UnverifiedClaim,
    /// Quote verified and measurements match expected policy (later).
    Verified {
        /// Placeholder: measurement digest matched allowlist.
        measurement_ok: bool,
    },
}

impl AttestationStatus {
    pub fn is_verified_tee(self) -> bool {
        matches!(
            self,
            AttestationStatus::Verified {
                measurement_ok: true
            }
        )
    }

    /// MVP helper: never treat unverified claims as TEE.
    pub fn from_peer_claim(claimed_tee: bool, quote_verified: bool, measurement_ok: bool) -> Self {
        if !claimed_tee {
            return AttestationStatus::None;
        }
        if !quote_verified {
            return AttestationStatus::UnverifiedClaim;
        }
        AttestationStatus::Verified { measurement_ok }
    }
}
