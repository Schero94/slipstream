//! Map `p2p-crypto::SealedEnvelope` ↔ `p2p-net::NetMessage::EncryptedJob`.
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
    let ciphertext = hex::decode(&sealed.ciphertext_hex)
        .map_err(|e| WireError::Hex(e.to_string()))?;
    let ephemeral_pubkey = hex::decode(&sealed.eph_pub_hex)
        .map_err(|e| WireError::Hex(e.to_string()))?;
    if ephemeral_pubkey.len() != 32 {
        return Err(WireError::BadEphKey);
    }
    Ok(NetMessage::EncryptedJob {
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
}
