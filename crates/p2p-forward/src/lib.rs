//! `p2p-forward` — v0.6 "Use your other Mac" forwarder (docs/plans/2026-11-28-use-your-other-mac.md).
//!
//! **M0 transport spike only.** This crate currently contains a *raw* byte pipe
//! (local TCP ⇄ one libp2p QUIC stream ⇄ upstream TCP), optionally wrapped in
//! [`p2p_crypto::SealedStream`], plus the measurement helpers used by
//! `bench/forward/`. The raw pipe has no allowlist and no trust gate and must never be
//! exposed beyond loopback; M1 replaces it with the allowlisted L7 forwarder.

#![forbid(unsafe_code)]

pub mod bench;
pub mod mock;
pub mod node;
pub mod pipe;

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
