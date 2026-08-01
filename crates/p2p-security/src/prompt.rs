//! Prompt confidentiality limits (TM-001) — honest about worker visibility.
//!
//! Transit AEAD does **not** hide prompts from the selected worker. This policy
//! blocks remote routing of Secret material unless a verified attestation path
//! is present (future TEE). See `docs/p2p/THREAT_MODEL.md`.

use crate::attestation::{AttestationStatus, TeeRequirement};

/// How sensitive the prompt body is treated for routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptSensitivity {
    /// Ok to route to allowlisted remote peers.
    Public,
    /// Prefer local; remote only with explicit override.
    Sensitive,
    /// Local-only unless verified TEE attestation (later).
    Secret,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteTarget {
    Local,
    Remote,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptPolicyError {
    /// Secret/Sensitive blocked for unattested remote worker.
    RemoteExfilRisk {
        sensitivity: PromptSensitivity,
        attestation: AttestationStatus,
    },
    ExplicitOverrideRequired,
}

/// Routing gate that encodes confidentiality assumptions.
#[derive(Debug, Clone)]
pub struct PromptPolicy {
    /// When true, Sensitive may remote without TEE (operator override).
    pub allow_sensitive_remote_override: bool,
}

impl Default for PromptPolicy {
    fn default() -> Self {
        Self {
            allow_sensitive_remote_override: false,
        }
    }
}

impl PromptPolicy {
    /// Decide whether a route is allowed given sensitivity + attestation.
    ///
    /// Invariant (MVP): a remote worker that can `open()` the envelope **will**
    /// see plaintext. Mitigations are routing policy + optional future TEE,
    /// not stronger transit crypto alone.
    pub fn allow_route(
        &self,
        sensitivity: PromptSensitivity,
        target: RouteTarget,
        attestation: AttestationStatus,
        tee: TeeRequirement,
    ) -> Result<(), PromptPolicyError> {
        if target == RouteTarget::Local {
            return Ok(());
        }

        match sensitivity {
            PromptSensitivity::Public => Ok(()),
            PromptSensitivity::Sensitive => {
                if attestation.is_verified_tee() && tee != TeeRequirement::Forbidden {
                    return Ok(());
                }
                if self.allow_sensitive_remote_override {
                    return Ok(());
                }
                Err(PromptPolicyError::RemoteExfilRisk {
                    sensitivity,
                    attestation,
                })
            }
            PromptSensitivity::Secret => {
                // Secret never remotes on Attestation::None (TEE optional → absent).
                if attestation.is_verified_tee() && tee == TeeRequirement::Required {
                    Ok(())
                } else {
                    Err(PromptPolicyError::RemoteExfilRisk {
                        sensitivity,
                        attestation,
                    })
                }
            }
        }
    }
}
