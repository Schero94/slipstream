//! Wire messages for the token-torrent MVP transport.
//!
//! These are intentionally transport-local. When `p2p-core` / `p2p-crypto` stabilize,
//! map [`CapabilityAdvert`] / encrypted job blobs at the crate boundary (`TODO(core)`).

use serde::{Deserialize, Serialize};

/// Lightweight capability advertisement carried in [`NetMessage::Hello`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityAdvert {
    pub node_id: String,
    pub models: Vec<String>,
    pub ram_gib: u32,
    pub vram_gib: u32,
    /// e.g. `"mock"`, `"mlx"`, `"llama_pgrn"`
    pub backend: String,
}

/// Length-prefixed JSON frames exchanged over TCP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NetMessage {
    /// First message after connect: identity + capability.
    Hello { capability: CapabilityAdvert },
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
    /// Worker → client inference result (cleartext model output for MVP).
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
        let result = NetMessage::JobResult {
            job_id: "j-1".into(),
            ok: true,
            text: "pong".into(),
            tokens: 1,
            error: None,
        };
        assert_eq!(NetMessage::decode(&job.encode().unwrap()).unwrap(), job);
        assert_eq!(
            NetMessage::decode(&result.encode().unwrap()).unwrap(),
            result
        );
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
                capability: sample_cap()
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
