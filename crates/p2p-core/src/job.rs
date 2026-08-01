//! Job request / result messages shared across the mesh.

use serde::{Deserialize, Serialize};

use crate::identity::NodeId;

/// Plaintext inference job (encrypted in transit by `p2p-crypto`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobRequest {
    pub job_id: String,
    pub model: String,
    pub system: String,
    pub prompt: String,
    pub max_tokens: u32,
}

/// Inference outcome returned to the requester.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobResult {
    pub job_id: String,
    pub text: String,
    pub tokens: u32,
    pub ok: bool,
    pub error: Option<String>,
}

impl JobResult {
    pub fn success(job_id: impl Into<String>, text: impl Into<String>, tokens: u32) -> Self {
        Self {
            job_id: job_id.into(),
            text: text.into(),
            tokens,
            ok: true,
            error: None,
        }
    }

    pub fn failure(job_id: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            job_id: job_id.into(),
            text: String::new(),
            tokens: 0,
            ok: false,
            error: Some(error.into()),
        }
    }
}

/// Envelope metadata for routing / settlement (payload sealing is `p2p-crypto`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobEnvelopeMeta {
    pub job_id: String,
    pub from: NodeId,
    pub to: NodeId,
    pub model: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_serde_roundtrip() {
        let req = JobRequest {
            job_id: "j1".into(),
            model: "mock".into(),
            system: "sys".into(),
            prompt: "hi".into(),
            max_tokens: 16,
        };
        let bytes = serde_json::to_vec(&req).unwrap();
        let back: JobRequest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn job_result_helpers() {
        let ok = JobResult::success("j1", "hello", 2);
        assert!(ok.ok);
        assert_eq!(ok.tokens, 2);
        let err = JobResult::failure("j1", "boom");
        assert!(!err.ok);
        assert_eq!(err.error.as_deref(), Some("boom"));
    }
}
