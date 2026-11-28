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
pub mod upstream;
pub mod worker;

pub use client::{start_use, AddrBook, LocalLimits, UseConfig, UseDiscovery, UseEvent, UseHandle};
pub use trust::{PeerTrust, Role, StaticTrust, TrustEpoch};
pub use worker::{parse_upstream, start_share, ShareConfig, ShareHandle, ShareLimits};
pub use status::{ModelControl, ReplyFuture, Status, StatusReply, StatusRequest};
/// Re-exported so embedders (the app) need no direct libp2p dependency.
pub use libp2p::{Multiaddr, PeerId};

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
    use std::io::Read;
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
            let token = new_token();
            // Publish atomically (temp file 0600 → hard link): a concurrent reader never sees
            // an empty file; if another process won the race, use its token (review "info").
            match publish_token(path, &token, false) {
                Ok(()) => Ok(token),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => load_or_create_token(path),
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}

fn new_token() -> String {
    let mut bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
    hex::encode(bytes)
}

/// Write `token` to a fresh temp file (0600, `create_new`, fsync) next to `path`, then
/// publish it: `replace = false` → hard link (fails with `AlreadyExists` if `path` exists),
/// `replace = true` → rename over `path`.
fn publish_token(path: &Path, token: &str, replace: bool) -> std::io::Result<()> {
    use std::io::Write;
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut rnd = [0u8; 8];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut rnd);
    let tmp = parent.join(format!(".local.token.tmp-{}-{}", std::process::id(), hex::encode(rnd)));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let result = (|| {
        let mut f = opts.open(&tmp)?;
        f.write_all(format!("{token}\n").as_bytes())?;
        f.sync_all()?;
        if replace {
            std::fs::rename(&tmp, path)
        } else {
            std::fs::hard_link(&tmp, path)
        }
    })();
    let _ = std::fs::remove_file(&tmp);
    result
}

/// A new random token replaces the one in `path` (atomically, 0600). Every editor config
/// that carries the old one must be patched again (the app does this; review M2
/// "regenerate token").
pub fn regenerate_token(path: &Path) -> std::io::Result<String> {
    let token = new_token();
    publish_token(path, &token, true)?;
    Ok(token)
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
        // regenerate: a new value, still 0600, no temp files left
        let r = regenerate_token(&path).unwrap();
        assert_ne!(r, t);
        assert_eq!(load_or_create_token(&path).unwrap(), r);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from("local.token")]);
    }

    /// Review "info": many threads creating the token at once all get the same, complete
    /// value (no empty-file read, no error on AlreadyExists).
    #[test]
    fn concurrent_token_creation_agrees() {
        for _ in 0..20 {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("local.token");
            let got: Vec<String> = std::thread::scope(|s| {
                let hs: Vec<_> = (0..8).map(|_| s.spawn(|| load_or_create_token(&path).unwrap())).collect();
                hs.into_iter().map(|h| h.join().unwrap()).collect()
            });
            assert!(got.iter().all(|t| t == &got[0] && t.len() == 64), "{got:?}");
        }
    }
}
