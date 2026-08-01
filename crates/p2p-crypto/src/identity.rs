//! X25519 node keypairs: generate, load/save, and map to [`p2p_core::NodeId`].

use std::fs;
use std::io::{Read, Write};
use std::path::Path;

use p2p_core::NodeId;
use rand::rngs::OsRng;
use thiserror::Error;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::{Zeroize, ZeroizeOnDrop};

/// Long-lived X25519 identity for a mesh node.
///
/// The public key bytes (hex) are the node's [`NodeId`]. The secret never leaves
/// the process except via explicit [`Self::save`] / [`Self::secret_bytes`].
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct NodeKeypair {
    #[zeroize(skip)]
    secret: StaticSecret,
    #[zeroize(skip)]
    public: PublicKey,
}

impl NodeKeypair {
    /// Fresh random static keypair (`OsRng`).
    pub fn generate() -> Self {
        let secret = StaticSecret::random_from_rng(OsRng);
        let public = PublicKey::from(&secret);
        Self { secret, public }
    }

    /// Deterministic keypair from a 32-byte seed/secret (tests + bring-your-own-key).
    pub fn from_secret_bytes(bytes: [u8; 32]) -> Self {
        let secret = StaticSecret::from(bytes);
        let public = PublicKey::from(&secret);
        Self { secret, public }
    }

    /// Load a keypair from disk.
    ///
    /// Accepted formats:
    /// - exactly 32 raw bytes, or
    /// - 64 hex characters (optional surrounding whitespace / trailing newline).
    pub fn load(path: &Path) -> Result<Self, IdentityIoError> {
        let mut file = fs::File::open(path).map_err(IdentityIoError::Io)?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).map_err(IdentityIoError::Io)?;
        let secret = parse_secret_bytes(&buf)?;
        Ok(Self::from_secret_bytes(secret))
    }

    /// Persist the 32-byte secret as lowercase hex + newline (0600 on Unix).
    pub fn save(&self, path: &Path) -> Result<(), IdentityIoError> {
        let hex = hex::encode(self.secret.to_bytes());
        write_secret_file(path, format!("{hex}\n").as_bytes())?;
        Ok(())
    }

    pub fn public_bytes(&self) -> [u8; 32] {
        *self.public.as_bytes()
    }

    pub fn public_hex(&self) -> String {
        hex::encode(self.public_bytes())
    }

    /// Mesh node id = hex encoding of the X25519 public key.
    pub fn node_id(&self) -> NodeId {
        NodeId::from_bytes(&self.public_bytes())
    }

    pub fn public_key(&self) -> PublicKey {
        self.public
    }

    pub(crate) fn secret(&self) -> &StaticSecret {
        &self.secret
    }

    /// Copy of the static secret (prefer [`Self::save`] for persistence).
    pub fn secret_bytes(&self) -> [u8; 32] {
        self.secret.to_bytes()
    }
}

impl std::fmt::Debug for NodeKeypair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeKeypair")
            .field("public_hex", &self.public_hex())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Error)]
pub enum IdentityIoError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("secret file must be 32 raw bytes or 64 hex chars")]
    BadSecretFormat,
}

fn parse_secret_bytes(buf: &[u8]) -> Result<[u8; 32], IdentityIoError> {
    if buf.len() == 32 {
        let mut out = [0u8; 32];
        out.copy_from_slice(buf);
        return Ok(out);
    }
    let text = std::str::from_utf8(buf)
        .map_err(|_| IdentityIoError::BadSecretFormat)?
        .trim();
    let bytes = hex::decode(text).map_err(|_| IdentityIoError::BadSecretFormat)?;
    if bytes.len() != 32 {
        return Err(IdentityIoError::BadSecretFormat);
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn write_secret_file(path: &Path, data: &[u8]) -> Result<(), IdentityIoError> {
    let mut file = fs::File::create(path).map_err(IdentityIoError::Io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
        file.set_permissions(perms).map_err(IdentityIoError::Io)?;
    }
    file.write_all(data).map_err(IdentityIoError::Io)?;
    file.sync_all().map_err(IdentityIoError::Io)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn generate_unique_public_keys() {
        let a = NodeKeypair::generate();
        let b = NodeKeypair::generate();
        assert_ne!(a.public_bytes(), b.public_bytes());
        assert_eq!(a.node_id().as_hex().len(), 64);
    }

    #[test]
    fn from_secret_bytes_deterministic() {
        let a = NodeKeypair::from_secret_bytes([7u8; 32]);
        let b = NodeKeypair::from_secret_bytes([7u8; 32]);
        assert_eq!(a.public_hex(), b.public_hex());
        assert_eq!(a.node_id(), b.node_id());
    }

    #[test]
    fn load_save_hex_roundtrip() {
        let kp = NodeKeypair::from_secret_bytes([9u8; 32]);
        let tmp = NamedTempFile::new().unwrap();
        kp.save(tmp.path()).unwrap();
        let loaded = NodeKeypair::load(tmp.path()).unwrap();
        assert_eq!(loaded.public_bytes(), kp.public_bytes());
        assert_eq!(loaded.secret_bytes(), kp.secret_bytes());
    }

    #[test]
    fn load_raw_32_bytes() {
        let path = NamedTempFile::new().unwrap();
        fs::write(path.path(), [3u8; 32]).unwrap();
        let loaded = NodeKeypair::load(path.path()).unwrap();
        assert_eq!(
            loaded.public_bytes(),
            NodeKeypair::from_secret_bytes([3u8; 32]).public_bytes()
        );
    }
}
