//! Node identity (opaque public id). Cryptographic key material lives in `p2p-crypto`.

use serde::{Deserialize, Serialize};

/// Stable node identifier: 32-byte public key material encoded as lowercase hex (64 chars).
///
/// `p2p-core` treats this as an opaque id. Sibling `p2p-crypto` owns X25519 generation
/// and sealed envelopes; it should produce/consume the same hex encoding.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(String);

impl NodeId {
    /// Construct from a 32-byte public key (or any stable 32-byte fingerprint).
    pub fn from_bytes(bytes: &[u8; 32]) -> Self {
        Self(hex::encode(bytes))
    }

    /// Parse a lowercase/uppercase hex string (must decode to exactly 32 bytes).
    pub fn from_hex(hex_str: &str) -> Result<Self, IdentityError> {
        let bytes = hex::decode(hex_str).map_err(|_| IdentityError::BadHex)?;
        if bytes.len() != 32 {
            return Err(IdentityError::BadLength);
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Ok(Self::from_bytes(&arr))
    }

    pub fn as_hex(&self) -> &str {
        &self.0
    }

    pub fn to_bytes(&self) -> [u8; 32] {
        let bytes = hex::decode(&self.0).expect("NodeId invariant: valid hex");
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        arr
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Local node identity handle used by the in-process API.
///
/// For MVP tests this is a random 32-byte id. Production nodes should use
/// `p2p-crypto` keypairs and convert the public key via [`NodeId::from_bytes`].
#[derive(Debug, Clone)]
pub struct NodeIdentity {
    id: NodeId,
    /// Opaque secret material (not used by core; reserved for crypto sibling wiring).
    secret: [u8; 32],
}

impl NodeIdentity {
    pub fn generate() -> Self {
        let mut secret = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut secret);
        // Public id for core tests: hash-like fingerprint of secret (not X25519).
        // Crypto sibling replaces this with a real public key.
        let mut id_bytes = secret;
        for b in &mut id_bytes {
            *b ^= 0x5a;
        }
        Self {
            id: NodeId::from_bytes(&id_bytes),
            secret,
        }
    }

    pub fn from_secret_bytes(secret: [u8; 32]) -> Self {
        let mut id_bytes = secret;
        for b in &mut id_bytes {
            *b ^= 0x5a;
        }
        Self {
            id: NodeId::from_bytes(&id_bytes),
            secret,
        }
    }

    pub fn id(&self) -> &NodeId {
        &self.id
    }

    pub fn secret_bytes(&self) -> &[u8; 32] {
        &self.secret
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IdentityError {
    #[error("invalid hex encoding for NodeId")]
    BadHex,
    #[error("NodeId must be 32 bytes")]
    BadLength,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_id_roundtrip_hex() {
        let id = NodeId::from_bytes(&[9u8; 32]);
        assert_eq!(id.as_hex().len(), 64);
        let parsed = NodeId::from_hex(id.as_hex()).unwrap();
        assert_eq!(parsed, id);
        assert_eq!(parsed.to_bytes(), [9u8; 32]);
    }

    #[test]
    fn node_id_rejects_bad_length() {
        assert_eq!(NodeId::from_hex("aa"), Err(IdentityError::BadLength));
    }

    #[test]
    fn identity_deterministic_from_secret() {
        let a = NodeIdentity::from_secret_bytes([1u8; 32]);
        let b = NodeIdentity::from_secret_bytes([1u8; 32]);
        assert_eq!(a.id(), b.id());
    }

    #[test]
    fn identity_generate_unique() {
        let a = NodeIdentity::generate();
        let b = NodeIdentity::generate();
        assert_ne!(a.id(), b.id());
    }
}
