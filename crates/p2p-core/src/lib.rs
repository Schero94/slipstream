//! `p2p-core` — stable shared types for the Slipstream P2P inference MVP.
//!
//! # Ownership
//! - **This crate:** [`NodeId`], [`NodeIdentity`], [`Capability`], [`BackendKind`],
//!   [`JobRequest`], [`JobResult`], [`InferenceEngine`] / [`MockEngine`], [`LocalNode`].
//! - **Siblings:** `p2p-crypto` (sealed envelopes), `p2p-router`, `p2p-ledger`,
//!   `p2p-engine` (MLX/llama adapters), `p2p-net`, `p2p-sim`.
//!
//! # Hardware gate
//! Providers must meet [`meets_min_hardware`]: ≥32 GiB RAM **or** ≥16 GiB VRAM.
//!
//! # Backend selection
//! [`select_backend`]: macOS/Darwin/iOS → [`BackendKind::Mlx`]; else → [`BackendKind::LlamaPgrn`].

#![forbid(unsafe_code)]

pub mod capability;
pub mod engine;
pub mod identity;
pub mod job;
pub mod node;

pub use capability::{
    local_capability, meets_min_hardware, select_backend, BackendKind, Capability, MIN_RAM_GIB,
    MIN_VRAM_GIB,
};
pub use engine::{InferenceEngine, MockEngine};
pub use identity::{IdentityError, NodeId, NodeIdentity};
pub use job::{JobEnvelopeMeta, JobRequest, JobResult};
pub use node::{LocalNode, LocalNodeConfig, PeerRecord};
