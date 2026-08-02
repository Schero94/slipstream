//! Wire messages for the token-torrent MVP transport.
//!
//! These are intentionally transport-local. When `p2p-core` / `p2p-crypto` stabilize,
//! map [`CapabilityAdvert`] / encrypted job blobs at the crate boundary (`TODO(core)`).

use serde::{Deserialize, Serialize};

/// Lightweight capability advertisement carried in [`NetMessage::Hello`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityAdvert {
    /// X25519 encryption key id used by sealed inference envelopes.
    pub node_id: String,
    /// Ed25519 identity public key id that signed this capability.
    #[serde(default)]
    pub identity_id: String,
    pub models: Vec<String>,
    pub ram_gib: u32,
    pub vram_gib: u32,
    /// e.g. `"mock"`, `"mlx"`, `"llama_pgrn"`
    pub backend: String,
}

/// Authentication metadata for a product Hello. The signature covers this
/// metadata (except `signature`) and the complete [`CapabilityAdvert`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloAuth {
    pub protocol: String,
    pub identity_pubkey: Vec<u8>,
    pub issued_at_unix: u64,
    pub expires_at_unix: u64,
    pub nonce: Vec<u8>,
    pub response_to: Vec<u8>,
    pub signature: Vec<u8>,
}

impl HelloAuth {
    pub fn signing_payload(
        &self,
        capability: &CapabilityAdvert,
    ) -> Result<Vec<u8>, serde_json::Error> {
        #[derive(Serialize)]
        struct SigningPayload<'a> {
            protocol: &'a str,
            capability: &'a CapabilityAdvert,
            identity_pubkey: &'a [u8],
            issued_at_unix: u64,
            expires_at_unix: u64,
            nonce: &'a [u8],
            response_to: &'a [u8],
        }

        serde_json::to_vec(&SigningPayload {
            protocol: &self.protocol,
            capability,
            identity_pubkey: &self.identity_pubkey,
            issued_at_unix: self.issued_at_unix,
            expires_at_unix: self.expires_at_unix,
            nonce: &self.nonce,
            response_to: &self.response_to,
        })
    }
}

/// Length-prefixed JSON frames exchanged over TCP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NetMessage {
    /// First message after connect: identity + capability.
    Hello {
        capability: CapabilityAdvert,
        /// `None` exists only for transport-unit compatibility. Product
        /// sessions require and verify authenticated Hello metadata.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auth: Option<HelloAuth>,
    },
    /// Opaque sealed job blob (ciphertext produced by `p2p-crypto`).
    EncryptedJob {
        job_id: String,
        /// Opaque AEAD ciphertext (+ tag); transport does not interpret.
        ciphertext: Vec<u8>,
        /// Nonce / IV bytes for the AEAD.
        nonce: Vec<u8>,
        /// Sender ephemeral X25519 public key (32 bytes when real crypto is wired).
        ephemeral_pubkey: Vec<u8>,
    },
    /// Worker → client sealed inference result (`p2p-crypto::seal_job_result`).
    EncryptedJobResult {
        job_id: String,
        /// Opaque AEAD ciphertext (+ tag); transport does not interpret.
        ciphertext: Vec<u8>,
        /// Nonce / IV bytes for the AEAD (empty when HKDF-derived, same as EncryptedJob).
        nonce: Vec<u8>,
        /// Sender ephemeral X25519 public key (32 bytes).
        ephemeral_pubkey: Vec<u8>,
    },
    /// Cleartext result — **loopback / transport-unit tests only** (TM-007).
    /// Product respond path must send [`Self::EncryptedJobResult`].
    JobResult {
        job_id: String,
        ok: bool,
        text: String,
        tokens: u32,
        error: Option<String>,
    },
    /// Keepalive / liveness probe.
    Heartbeat { seq: u64 },
}

impl NetMessage {
    /// Stable wire `type` tag — use in protocol errors so logs never dump ciphertext.
    pub fn wire_type(&self) -> &'static str {
        match self {
            Self::Hello { .. } => "hello",
            Self::EncryptedJob { .. } => "encrypted_job",
            Self::EncryptedJobResult { .. } => "encrypted_job_result",
            Self::JobResult { .. } => "job_result",
            Self::Heartbeat { .. } => "heartbeat",
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_cap() -> CapabilityAdvert {
        CapabilityAdvert {
            node_id: "node-a".into(),
            identity_id: String::new(),
            models: vec!["mock-7b".into()],
            ram_gib: 32,
            vram_gib: 0,
            backend: "mock".into(),
        }
    }

    #[test]
    fn hello_roundtrip() {
        let msg = NetMessage::Hello {
            capability: sample_cap(),
            auth: None,
        };
        let bytes = msg.encode().unwrap();
        let back = NetMessage::decode(&bytes).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn encrypted_job_and_result_roundtrip() {
        let job = NetMessage::EncryptedJob {
            job_id: "j-1".into(),
            ciphertext: b"sealed".to_vec(),
            nonce: vec![1, 2, 3],
            ephemeral_pubkey: vec![9; 32],
        };
        let sealed_result = NetMessage::EncryptedJobResult {
            job_id: "j-1".into(),
            ciphertext: b"sealed-out".to_vec(),
            nonce: vec![],
            ephemeral_pubkey: vec![8; 32],
        };
        // Cleartext JobResult remains for loopback/unit-test exception (TM-007).
        let clear = NetMessage::JobResult {
            job_id: "j-1".into(),
            ok: true,
            text: "pong".into(),
            tokens: 1,
            error: None,
        };
        assert_eq!(NetMessage::decode(&job.encode().unwrap()).unwrap(), job);
        assert_eq!(
            NetMessage::decode(&sealed_result.encode().unwrap()).unwrap(),
            sealed_result
        );
        assert_eq!(NetMessage::decode(&clear.encode().unwrap()).unwrap(), clear);
    }

    #[test]
    fn heartbeat_roundtrip() {
        let msg = NetMessage::Heartbeat { seq: 7 };
        assert_eq!(NetMessage::decode(&msg.encode().unwrap()).unwrap(), msg);
    }

    #[test]
    fn wire_type_tags_match_serde() {
        assert_eq!(
            NetMessage::Hello {
                capability: sample_cap(),
                auth: None,
            }
            .wire_type(),
            "hello"
        );
        assert_eq!(
            NetMessage::EncryptedJob {
                job_id: "j".into(),
                ciphertext: vec![],
                nonce: vec![],
                ephemeral_pubkey: vec![],
            }
            .wire_type(),
            "encrypted_job"
        );
        assert_eq!(
            NetMessage::EncryptedJobResult {
                job_id: "j".into(),
                ciphertext: vec![],
                nonce: vec![],
                ephemeral_pubkey: vec![],
            }
            .wire_type(),
            "encrypted_job_result"
        );
        assert_eq!(
            NetMessage::JobResult {
                job_id: "j".into(),
                ok: true,
                text: String::new(),
                tokens: 0,
                error: None,
            }
            .wire_type(),
            "job_result"
        );
        assert_eq!(NetMessage::Heartbeat { seq: 0 }.wire_type(), "heartbeat");
    }
}
