//! Model weight integrity pins (TM-002).
//!
//! MVP: refuse to serve/route a model whose content digest is not on the
//! allowlist. Does not download weights — only checks digests.

use std::collections::HashMap;

use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ModelPinError {
    #[error("unknown model id (not pinned)")]
    UnknownModel,
    #[error("model digest mismatch")]
    DigestMismatch,
}

/// Hex-encoded SHA-256 of model artifact bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelDigest(String);

impl ModelDigest {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let hash = Sha256::digest(bytes);
        Self(hex::encode(hash))
    }

    pub fn from_hex(hex_str: impl Into<String>) -> Self {
        Self(hex_str.into())
    }

    pub fn as_hex(&self) -> &str {
        &self.0
    }
}

/// Expected digests keyed by model id (e.g. "qwen3-30b").
#[derive(Debug, Clone, Default)]
pub struct ModelRegistry {
    pins: HashMap<String, ModelDigest>,
}

impl ModelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn pin(&mut self, model_id: impl Into<String>, digest: ModelDigest) {
        self.pins.insert(model_id.into(), digest);
    }

    /// Verify advertised/loaded bytes match the pin for `model_id`.
    pub fn verify(&self, model_id: &str, actual: &ModelDigest) -> Result<(), ModelPinError> {
        let expected = self.pins.get(model_id).ok_or(ModelPinError::UnknownModel)?;
        if expected != actual {
            return Err(ModelPinError::DigestMismatch);
        }
        Ok(())
    }
}
