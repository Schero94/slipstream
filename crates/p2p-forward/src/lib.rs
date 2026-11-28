//! `p2p-forward` — v0.6 "Use your other Mac" forwarder (docs/plans/2026-11-28-use-your-other-mac.md).
//!
//! M1: the allowlisted L7 forwarder over an authenticated, persistent QUIC connection.
//!
//! - [`proto`]: `/slipstream/openai/1` + `/slipstream/status/1`, SealedStream + framing
//! - [`allowlist`]: method/path allowlist (both ends), header stripping, `/slots` sanitizing
//! - [`client`]: use side (editor on `127.0.0.1:<port>`), [`worker`]: share side
//! - [`admission`]: per-peer 1 in flight + 2 queued, [`status`]: status RPC
//! - [`mock_llama`]: deterministic llama-server mock for tests and `bench/forward/run_m1.sh`
//! - M2: [`trust`] ([`PeerTrust`]: pinned static keys, pairing window, revoke signal) and
//!   [`gate`] (share-side trust gate at `ConnectionEstablished` + on every trust change);
//!   the trust store and pairing themselves live in `apps/p2p-node` (`trust.rs`, `pairing.rs`)
//!
//! [`node`] / [`pipe`] are the M0 raw byte pipe, kept only to reproduce the M0 numbers
//! (`forward-spike`, `bench/forward/run_m0.sh`); it has no allowlist and is never used
//! by `slipstream-node share|use`.

#![forbid(unsafe_code)]

pub mod admission;
pub mod allowlist;
pub mod bench;
pub mod client;
pub mod gate;
pub mod http;
pub mod mock;
pub mod mock_llama;
pub mod node;
pub mod pipe;
pub mod proto;
pub mod status;
pub mod trust;
pub mod worker;

pub use client::{start_use, AddrBook, LocalLimits, UseConfig, UseDiscovery, UseEvent, UseHandle};
pub use trust::{PeerTrust, StaticTrust, TrustEpoch};
pub use worker::{parse_upstream, start_share, ShareConfig, ShareHandle, ShareLimits};

pub fn unix_ms() -> u128 {
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
    Ok(transport_identity(&load_or_create_node(dir)?))
}

/// Load (or create, 0600) the node's static X25519 key `dir/node.key` — the root of
/// both the libp2p identity ([`transport_identity`]) and the paired static key used by
/// the authenticated SealedStream (v0.6 M2).
pub fn load_or_create_node(dir: &Path) -> std::io::Result<NodeKeypair> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("node.key");
    if path.exists() {
        NodeKeypair::load(&path).map_err(std::io::Error::other)
    } else {
        let node = NodeKeypair::generate();
        node.save(&path).map_err(std::io::Error::other)?;
        Ok(node)
    }
}

pub fn transport_identity(node: &NodeKeypair) -> libp2p::identity::Keypair {
    let signing = SigningIdentity::from_node_keypair(node);
    let mut seed = signing.secret_seed();
    let identity = libp2p::identity::Keypair::ed25519_from_bytes(&mut seed)
        .expect("32-byte Ed25519 seed is always valid");
    seed.fill(0);
    identity
}

/// Load the per-install local bearer token from `path`, or create it: 32 random bytes as
/// 64 lowercase hex characters, written to a new file opened with mode 0600 and
/// `create_new` (no permission window, never follows an existing file or symlink).
///
/// The use side requires `Authorization: Bearer <token>` on its loopback listener when a
/// token is configured (`slipstream-node use --token-file <path>`). The app (M4) creates
/// this file and writes the token into the editor configs as their "API key", so other
/// local processes / users cannot use the paired Mac through the forwarder.
pub fn load_or_create_token(path: &Path) -> std::io::Result<String> {
    use std::io::{Read, Write};
    match std::fs::File::open(path) {
        Ok(mut f) => {
            let mut s = String::new();
            Read::by_ref(&mut f).take(4096).read_to_string(&mut s)?;
            let token = s.trim().to_string();
            if token.len() < 16 || !token.bytes().all(|b| b.is_ascii_graphic()) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{}: token must be at least 16 visible ASCII characters", path.display()),
                ));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = f.metadata()?.permissions().mode();
                if mode & 0o077 != 0 {
                    eprintln!(
                        "warning: {} is readable by other users (mode {:o}); chmod 600 it",
                        path.display(),
                        mode & 0o777
                    );
                }
            }
            Ok(token)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut bytes = [0u8; 32];
            rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
            let token = hex::encode(bytes);
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)?;
            }
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut f = opts.open(path)?;
            f.write_all(format!("{token}\n").as_bytes())?;
            f.sync_all()?;
            Ok(token)
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_file_created_0600_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/local.token");
        let t = load_or_create_token(&path).unwrap();
        assert_eq!(t.len(), 64);
        assert!(t.bytes().all(|b| b.is_ascii_hexdigit()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(load_or_create_token(&path).unwrap(), t, "reused, not regenerated");
        let other = load_or_create_token(&dir.path().join("b.token")).unwrap();
        assert_ne!(other, t);
        std::fs::write(dir.path().join("short"), "abc\n").unwrap();
        assert!(load_or_create_token(&dir.path().join("short")).is_err());
    }
}
