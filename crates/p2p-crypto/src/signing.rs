//! Ed25519 identity derived with domain separation from the node root secret.

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use hkdf::Hkdf;
use sha2::Sha256;
use thiserror::Error;

use crate::NodeKeypair;

const IDENTITY_SALT: &[u8] = b"slipstream-identity-v1";
const IDENTITY_INFO: &[u8] = b"ed25519-signing-seed";

#[derive(Clone)]
pub struct SigningIdentity {
    signing: SigningKey,
}

impl SigningIdentity {
    /// Derive an independent Ed25519 seed from the persisted X25519 root secret.
    /// HKDF domain separation avoids reusing identical scalar/key material.
    pub fn from_node_keypair(node: &NodeKeypair) -> Self {
        let hkdf = Hkdf::<Sha256>::new(Some(IDENTITY_SALT), &node.secret_bytes());
        let mut seed = [0u8; 32];
        hkdf.expand(IDENTITY_INFO, &mut seed)
            .expect("32-byte identity HKDF output is valid");
        Self {
            signing: SigningKey::from_bytes(&seed),
        }
    }

    pub fn public_bytes(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    pub fn public_hex(&self) -> String {
        hex::encode(self.public_bytes())
    }

    pub fn sign(&self, payload: &[u8]) -> Vec<u8> {
        self.signing.sign(payload).to_bytes().to_vec()
    }
}

impl std::fmt::Debug for SigningIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SigningIdentity")
            .field("public_hex", &self.public_hex())
            .finish_non_exhaustive()
    }
}

pub fn verify_identity_signature(
    public_key: &[u8],
    payload: &[u8],
    signature: &[u8],
) -> Result<(), SigningError> {
    let public_key: [u8; 32] = public_key
        .try_into()
        .map_err(|_| SigningError::BadPublicKey)?;
    let signature = Signature::from_slice(signature).map_err(|_| SigningError::BadSignature)?;
    let verifying =
        VerifyingKey::from_bytes(&public_key).map_err(|_| SigningError::BadPublicKey)?;
    verifying
        .verify_strict(payload, &signature)
        .map_err(|_| SigningError::Verification)
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SigningError {
    #[error("invalid Ed25519 public key")]
    BadPublicKey,
    #[error("invalid Ed25519 signature encoding")]
    BadSignature,
    #[error("Ed25519 signature verification failed")]
    Verification,
}
