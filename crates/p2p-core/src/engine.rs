//! Inference engine trait + deterministic mock for CI.
//!
//! Real adapters (MLX / llama) live in `p2p-engine` and implement this trait.

use crate::capability::BackendKind;
use crate::job::{JobRequest, JobResult};

/// Pluggable inference backend. Sync for MVP; async wrappers can sit in siblings.
pub trait InferenceEngine: Send + Sync {
    fn infer(&self, job: &JobRequest) -> JobResult;

    /// Optional production backend hint (mock returns `None`).
    fn backend_kind(&self) -> Option<BackendKind> {
        None
    }
}

/// Deterministic mock engine — no GPU required.
pub struct MockEngine;

impl InferenceEngine for MockEngine {
    fn infer(&self, job: &JobRequest) -> JobResult {
        let n = job.max_tokens.max(1).min(64);
        let mut parts = Vec::with_capacity(n as usize);
        for i in 0..n {
            parts.push(format!("tok{i}"));
        }
        let text = format!(
            "[mock model={} sys_len={} prompt_hash={:x}] {}",
            job.model,
            job.system.len(),
            fnv1a32(job.prompt.as_bytes()),
            parts.join(" ")
        );
        JobResult::success(&job.job_id, text, n)
    }

    fn backend_kind(&self) -> Option<BackendKind> {
        None
    }
}

fn fnv1a32(data: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for b in data {
        hash ^= u32::from(*b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_engine_is_deterministic() {
        let eng = MockEngine;
        let job = JobRequest {
            job_id: "1".into(),
            model: "m".into(),
            system: "s".into(),
            prompt: "hello".into(),
            max_tokens: 3,
        };
        let a = eng.infer(&job);
        let b = eng.infer(&job);
        assert_eq!(a, b);
        assert!(a.ok);
        assert_eq!(a.tokens, 3);
        assert!(a.text.contains("tok0"));
        assert!(eng.backend_kind().is_none());
    }
}
