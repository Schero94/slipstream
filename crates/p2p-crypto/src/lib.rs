//! `p2p-crypto` — sealed job envelopes for Slipstream P2P token-torrent.
//!
//! # What this protects
//! Encrypts job payloads **in transit** between nodes using ephemeral X25519
//! ECDH → HKDF-SHA256 → ChaCha20-Poly1305 AEAD.
//!
//! # What this is NOT
//! This is **not** fully homomorphic encryption (FHE) and **not** blind
//! inference. The receiving node **decrypts plaintext in-process** before
//! running inference. A compromised or malicious provider node can read the
//! prompt. Protection is against network eavesdroppers / on-path observers.
//!
//! # Key ownership
//! - [`NodeKeypair`] — real X25519 static identity (this crate).
//! - [`p2p_core::NodeId`] — opaque public id (hex of the X25519 public key).
//! - [`p2p_core::NodeIdentity`] — core MVP stub (XOR fingerprint); prefer
//!   [`NodeKeypair`] + [`NodeKeypair::node_id`] for production mesh crypto.
//!   // TODO(core): merge key material ownership once core drops the stub id.
//!
//! # Typed job helpers
//! [`seal_job_request`] / [`open_job_request`], [`seal_job_result`] /
//! [`open_job_result`], and [`seal_job`] / [`open_job`] (meta + payload).

#![forbid(unsafe_code)]

mod envelope;
mod error;
mod identity;
mod signing;

pub use envelope::{
    open, open_job, open_job_request, open_job_result, open_json, public_key_from_hex, seal,
    seal_job, seal_job_request, seal_job_result, seal_json, seal_to_node_id, SealedEnvelope,
    SealedJob,
};
pub use error::CryptoError;
pub use identity::{IdentityIoError, NodeKeypair};
pub use signing::{verify_identity_signature, SigningError, SigningIdentity};
