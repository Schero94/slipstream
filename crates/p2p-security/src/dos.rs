//! Admission / DoS limits (TM-006).
//!
//! Complements `slipstream-mesh` `MAX_FRAME` (4 MiB) with job concurrency,
//! per-peer rate, and max_tokens caps.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionDecision {
    Allow,
    RejectConcurrentLimit,
    RejectTokenLimit,
    RejectPeerRate,
    RejectFrameLimit,
}

/// Configurable admission policy for a worker node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DosLimits {
    pub max_concurrent_jobs: u32,
    pub max_tokens_per_job: u32,
    pub max_jobs_per_peer_per_window: u32,
    /// Mirror mesh framing; default 4 MiB.
    pub max_frame_bytes: u32,
}

impl Default for DosLimits {
    fn default() -> Self {
        Self {
            max_concurrent_jobs: 4,
            max_tokens_per_job: 4096,
            max_jobs_per_peer_per_window: 16,
            max_frame_bytes: 4 * 1024 * 1024,
        }
    }
}

impl DosLimits {
    pub fn admit(
        &self,
        active_jobs: u32,
        peer_jobs_in_window: u32,
        max_tokens: u32,
        frame_bytes: u32,
    ) -> AdmissionDecision {
        if frame_bytes == 0 || frame_bytes > self.max_frame_bytes {
            return AdmissionDecision::RejectFrameLimit;
        }
        if active_jobs >= self.max_concurrent_jobs {
            return AdmissionDecision::RejectConcurrentLimit;
        }
        if max_tokens > self.max_tokens_per_job {
            return AdmissionDecision::RejectTokenLimit;
        }
        if peer_jobs_in_window >= self.max_jobs_per_peer_per_window {
            return AdmissionDecision::RejectPeerRate;
        }
        AdmissionDecision::Allow
    }
}
