//! Map `p2p-crypto::SealedEnvelope` ↔ `p2p-net::NetMessage::{EncryptedJob,EncryptedJobResult}`.
//!
//! `p2p-crypto` derives the AEAD nonce via HKDF (not sent on the wire). The
//! transport `nonce` field is left empty when sealing with real crypto.

use p2p_crypto::SealedEnvelope;
use p2p_net::NetMessage;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum WireError {
    #[error("invalid hex in sealed envelope: {0}")]
    Hex(String),
    #[error("encrypted job missing ephemeral pubkey (need 32 bytes)")]
    BadEphKey,
}

/// Pack a sealed job into a transport frame.
pub fn sealed_to_net(job_id: impl Into<String>, sealed: &SealedEnvelope) -> Result<NetMessage, WireError> {
    let (ciphertext, ephemeral_pubkey) = decode_sealed(sealed)?;
    Ok(NetMessage::EncryptedJob {
        job_id: job_id.into(),
        ciphertext,
        nonce: Vec::new(),
        ephemeral_pubkey,
    })
}

/// Pack a sealed [`p2p_core::JobResult`] into a transport frame (TM-007).
pub fn sealed_result_to_net(
    job_id: impl Into<String>,
    sealed: &SealedEnvelope,
) -> Result<NetMessage, WireError> {
    let (ciphertext, ephemeral_pubkey) = decode_sealed(sealed)?;
    Ok(NetMessage::EncryptedJobResult {
        job_id: job_id.into(),
        ciphertext,
        nonce: Vec::new(),
        ephemeral_pubkey,
    })
}

/// Unpack transport fields back into a [`SealedEnvelope`].
pub fn net_to_sealed(
    ciphertext: &[u8],
    ephemeral_pubkey: &[u8],
) -> Result<SealedEnvelope, WireError> {
    if ephemeral_pubkey.len() != 32 {
        return Err(WireError::BadEphKey);
    }
    Ok(SealedEnvelope {
        eph_pub_hex: hex::encode(ephemeral_pubkey),
        ciphertext_hex: hex::encode(ciphertext),
    })
}

fn decode_sealed(sealed: &SealedEnvelope) -> Result<(Vec<u8>, Vec<u8>), WireError> {
    let ciphertext = hex::decode(&sealed.ciphertext_hex)
        .map_err(|e| WireError::Hex(e.to_string()))?;
    let ephemeral_pubkey = hex::decode(&sealed.eph_pub_hex)
        .map_err(|e| WireError::Hex(e.to_string()))?;
    if ephemeral_pubkey.len() != 32 {
        return Err(WireError::BadEphKey);
    }
    Ok((ciphertext, ephemeral_pubkey))
}

#[cfg(test)]
mod tests {
    use super::*;
    use p2p_crypto::{open, seal, NodeKeypair};

    #[test]
    fn sealed_net_roundtrip() {
        let bob = NodeKeypair::generate();
        let sealed = seal(b"hello wire", &bob.public_key()).unwrap();
        let msg = sealed_to_net("j1", &sealed).unwrap();
        match msg {
            NetMessage::EncryptedJob {
                job_id,
                ciphertext,
                nonce,
                ephemeral_pubkey,
            } => {
                assert_eq!(job_id, "j1");
                assert!(nonce.is_empty());
                let back = net_to_sealed(&ciphertext, &ephemeral_pubkey).unwrap();
                assert_eq!(open(&back, &bob).unwrap(), b"hello wire");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn sealed_result_net_roundtrip() {
        let alice = NodeKeypair::generate();
        let sealed = seal(b"hello result", &alice.public_key()).unwrap();
        let msg = sealed_result_to_net("j1", &sealed).unwrap();
        match msg {
            NetMessage::EncryptedJobResult {
                job_id,
                ciphertext,
                nonce,
                ephemeral_pubkey,
            } => {
                assert_eq!(job_id, "j1");
                assert!(nonce.is_empty());
                let back = net_to_sealed(&ciphertext, &ephemeral_pubkey).unwrap();
                assert_eq!(open(&back, &alice).unwrap(), b"hello result");
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
