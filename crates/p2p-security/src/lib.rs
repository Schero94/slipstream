//! Slipstream P2P security contracts.
//!
//! These types encode MVP mitigations from `docs/p2p/THREAT_MODEL.md` as
//! small, testable policy helpers. They intentionally do **not** reimplement
//! wire crypto (owned by `slipstream-mesh` / future `p2p-crypto`).
//!
//! Attack scenarios live in `tests/attack_scenarios.rs` and assert that the
//! documented attacks fail when mitigations are applied.

#![forbid(unsafe_code)]

pub mod attestation;
pub mod dos;
pub mod model_integrity;
pub mod prompt;
pub mod replay;
pub mod settlement;
pub mod sybil;

pub use attestation::{AttestationStatus, TeeRequirement};
pub use dos::{AdmissionDecision, DosLimits};
pub use model_integrity::{ModelDigest, ModelPinError, ModelRegistry};
pub use prompt::{PromptPolicy, PromptSensitivity, RouteTarget};
pub use replay::{envelope_fingerprint, ReplayCache, ReplayError};
pub use settlement::{CompletionReceipt, SettlementError, SettlementGuard};
pub use sybil::{Allowlist, SybilError};
