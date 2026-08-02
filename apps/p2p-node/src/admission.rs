//! Process-wide inference admission for untrusted peers.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use p2p_security::{AdmissionDecision, DosLimits};

use crate::{NodeMode, NodePolicy};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionRejection {
    DonationDisabled,
    ConcurrentLimit,
    TokenLimit,
    PeerRate,
    FrameLimit,
}

impl AdmissionRejection {
    pub fn as_code(self) -> &'static str {
        match self {
            Self::DonationDisabled => "donation_disabled",
            Self::ConcurrentLimit => "concurrent_limit",
            Self::TokenLimit => "token_limit",
            Self::PeerRate => "peer_rate",
            Self::FrameLimit => "frame_limit",
        }
    }
}

#[derive(Default)]
struct AdmissionState {
    active_jobs: u32,
    peer_jobs: HashMap<String, VecDeque<Instant>>,
}

struct AdmissionInner {
    limits: DosLimits,
    mode: NodeMode,
    donate_capacity: bool,
    peer_window: Duration,
    state: Mutex<AdmissionState>,
}

#[derive(Clone)]
pub struct AdmissionController {
    inner: Arc<AdmissionInner>,
}

impl fmt::Debug for AdmissionController {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdmissionController")
            .field("limits", &self.inner.limits)
            .field("mode", &self.inner.mode)
            .field("donate_capacity", &self.inner.donate_capacity)
            .finish_non_exhaustive()
    }
}

impl AdmissionController {
    pub fn new(policy: NodePolicy) -> Self {
        Self {
            inner: Arc::new(AdmissionInner {
                limits: policy.limits,
                mode: policy.mode,
                donate_capacity: policy.donate_capacity,
                peer_window: Duration::from_secs(policy.peer_window_secs),
                state: Mutex::new(AdmissionState::default()),
            }),
        }
    }

    pub fn admit(
        &self,
        peer_id: &str,
        max_tokens: u32,
        frame_bytes: u32,
    ) -> Result<AdmissionPermit, AdmissionRejection> {
        if self.inner.mode == NodeMode::Community && !self.inner.donate_capacity {
            return Err(AdmissionRejection::DonationDisabled);
        }

        let now = Instant::now();
        let mut state = self.inner.state.lock().expect("admission state");
        let peer_jobs_in_window = {
            let jobs = state.peer_jobs.entry(peer_id.to_string()).or_default();
            while jobs
                .front()
                .is_some_and(|seen| now.duration_since(*seen) > self.inner.peer_window)
            {
                jobs.pop_front();
            }
            u32::try_from(jobs.len()).unwrap_or(u32::MAX)
        };

        let decision = self.inner.limits.admit(
            state.active_jobs,
            peer_jobs_in_window,
            max_tokens,
            frame_bytes,
        );
        let rejection = match decision {
            AdmissionDecision::Allow => None,
            AdmissionDecision::RejectConcurrentLimit => Some(AdmissionRejection::ConcurrentLimit),
            AdmissionDecision::RejectTokenLimit => Some(AdmissionRejection::TokenLimit),
            AdmissionDecision::RejectPeerRate => Some(AdmissionRejection::PeerRate),
            AdmissionDecision::RejectFrameLimit => Some(AdmissionRejection::FrameLimit),
        };
        if let Some(rejection) = rejection {
            return Err(rejection);
        }

        state
            .peer_jobs
            .entry(peer_id.to_string())
            .or_default()
            .push_back(now);
        state.active_jobs = state.active_jobs.saturating_add(1);
        drop(state);

        Ok(AdmissionPermit {
            inner: Arc::clone(&self.inner),
            released: false,
        })
    }

    /// Cheap pre-decryption frame gate. `admit` repeats this check after the
    /// request is opened so callers cannot accidentally bypass it.
    pub fn validate_frame(&self, frame_bytes: u32) -> Result<(), AdmissionRejection> {
        if frame_bytes == 0 || frame_bytes > self.inner.limits.max_frame_bytes {
            return Err(AdmissionRejection::FrameLimit);
        }
        Ok(())
    }

    pub fn active_jobs(&self) -> u32 {
        self.inner
            .state
            .lock()
            .expect("admission state")
            .active_jobs
    }
}

pub struct AdmissionPermit {
    inner: Arc<AdmissionInner>,
    released: bool,
}

impl fmt::Debug for AdmissionPermit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdmissionPermit")
            .field("released", &self.released)
            .finish_non_exhaustive()
    }
}

impl Drop for AdmissionPermit {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let mut state = self.inner.state.lock().expect("admission state");
        state.active_jobs = state.active_jobs.saturating_sub(1);
        self.released = true;
    }
}
