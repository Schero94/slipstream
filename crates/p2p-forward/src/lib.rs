//! `p2p-forward` — v0.6 "Use your other Mac" forwarder (docs/plans/2026-11-28-use-your-other-mac.md).
//!
//! M1: the allowlisted L7 forwarder over an authenticated, persistent QUIC connection.
//!
//! - [`proto`]: `/slipstream/openai/1` + `/slipstream/status/1`, SealedStream + framing
//! - [`allowlist`]: method/path allowlist (both ends), header stripping, `/slots` sanitizing
//! - [`client`]: use side (editor on `127.0.0.1:<port>`), [`worker`]: share side
//! - [`admission`]: per-peer 1 in flight + 2 queued, [`status`]: status RPC
//! - [`mock_llama`]: deterministic llama-server mock for tests and `bench/forward/run_m1.sh`
//!
//! [`node`] / [`pipe`] are the M0 raw byte pipe, kept only to reproduce the M0 numbers
//! (`forward-spike`, `bench/forward/run_m0.sh`); it has no allowlist and is never used
//! by `slipstream-node share|use`.

#![forbid(unsafe_code)]

pub mod admission;
pub mod allowlist;
pub mod bench;
pub mod client;
pub mod http;
pub mod mock;
pub mod mock_llama;
pub mod node;
pub mod pipe;
pub mod proto;
pub mod status;
pub mod worker;

pub use client::{start_use, UseConfig, UseHandle};
pub use worker::{parse_upstream, start_share, ShareConfig, ShareHandle};

pub(crate) fn unix_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

use std::path::Path;

use p2p_crypto::{NodeKeypair, SigningIdentity};

/// Load (or create, 0600) the node key in `dir/node.key` and derive the libp2p
/// Ed25519 transport identity exactly like `apps/p2p-node/src/mesh.rs` does, so the
/// PeerId equals the signed-Hello identity.
pub fn load_or_create_identity(dir: &Path) -> std::io::Result<libp2p::identity::Keypair> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("node.key");
    let node = if path.exists() {
        NodeKeypair::load(&path).map_err(std::io::Error::other)?
    } else {
        let node = NodeKeypair::generate();
        node.save(&path).map_err(std::io::Error::other)?;
        node
    };
    Ok(transport_identity(&node))
}

pub fn transport_identity(node: &NodeKeypair) -> libp2p::identity::Keypair {
    let signing = SigningIdentity::from_node_keypair(node);
    let mut seed = signing.secret_seed();
    let identity = libp2p::identity::Keypair::ed25519_from_bytes(&mut seed)
        .expect("32-byte Ed25519 seed is always valid");
    seed.fill(0);
    identity
}
