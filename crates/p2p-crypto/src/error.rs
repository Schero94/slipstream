use thiserror::Error;

/// Errors from sealing / opening envelopes (and JSON payload helpers).
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CryptoError {
    #[error("AEAD encrypt failed")]
    Encrypt,

    #[error("AEAD decrypt failed (wrong key or tampered ciphertext)")]
    Decrypt,

    #[error("invalid public key (expected 32 bytes)")]
    BadKey,

    #[error("HKDF expand failed")]
    Hkdf,

    #[error("JSON (de)serialization failed: {0}")]
    Serde(String),
}
