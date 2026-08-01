//! Replay hook for inbound [`NetMessage::EncryptedJob`] (TM-004).
//!
//! Uses `p2p_security::{ReplayCache, envelope_fingerprint}`. AEAD alone does
//! not stop re-submission of a captured valid envelope — callers that process
//! jobs must admit through [`admit_encrypted_job`] (or
//! [`crate::session::PeerSession::recv_with_replay`]).

use p2p_security::{envelope_fingerprint, ReplayCache, ReplayError};

use crate::error::NetError;
use crate::message::NetMessage;

/// Fingerprint an opaque wire job for the security replay cache.
///
/// Hex-encodes `ephemeral_pubkey` + `ciphertext` to align with
/// `p2p_crypto::SealedEnvelope::{eph_pub_hex, ciphertext_hex}` field form.
pub fn encrypted_job_fingerprint(
    ephemeral_pubkey: &[u8],
    ciphertext: &[u8],
) -> String {
    envelope_fingerprint(
        &hex::encode(ephemeral_pubkey),
        &hex::encode(ciphertext),
    )
}

/// Record / reject an inbound encrypted job against `cache`.
///
/// Non-`EncryptedJob` messages are ignored (Ok). Duplicate `job_id` within the
/// cache TTL → [`NetError::Replay`].
pub fn admit_encrypted_job(
    msg: &NetMessage,
    cache: &mut ReplayCache,
) -> Result<(), NetError> {
    let NetMessage::EncryptedJob {
        job_id,
        ciphertext,
        ephemeral_pubkey,
        ..
    } = msg
    else {
        return Ok(());
    };
    let fp = encrypted_job_fingerprint(ephemeral_pubkey, ciphertext);
    cache
        .accept(job_id, Some(&fp))
        .map_err(|e| match e {
            ReplayError::DuplicateJobId => NetError::Replay(format!(
                "duplicate job_id '{job_id}' (envelope already admitted in ReplayCache)"
            )),
        })
}
