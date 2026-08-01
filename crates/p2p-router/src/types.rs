//! Router-owned request/decision types. Core capability/job types come from `p2p-core`.

use serde::{Deserialize, Serialize};

use p2p_core::{Capability, JobRequest, NodeId};

/// Job / session context that influences sticky routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum JobContext {
    /// Default chat / one-shot.
    #[default]
    General,
    /// Multi-turn coding: prefer sticky peer for KV/session continuity.
    Coding,
}

/// Observable peer snapshot for the router (extends [`p2p_core::PeerRecord`] with live signals).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerSnapshot {
    /// Peer node id.
    pub node_id: NodeId,
    /// Dialable listen address.
    pub listen_addr: String,
    /// Advertised capability.
    pub capability: Capability,
    /// Round-trip latency estimate in milliseconds.
    pub rtt_ms: u32,
    /// Reputation stub in \[0, 100\].
    pub reputation: u32,
    /// Current load in \[0.0, 1.0\] (1.0 = saturated).
    pub load: f64,
    /// True when this peer is the local Slipstream node.
    pub is_local: bool,
}

/// Inputs for a route decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteRequest {
    /// Job whose model (and identity) drive selection.
    pub job: JobRequest,
    /// Session context (coding enables sticky preference).
    pub context: JobContext,
    /// Previous peer for sticky sessions (coding).
    pub sticky_peer: Option<NodeId>,
    /// Requester OS hint (affects backend preference via [`p2p_core::select_backend`]).
    pub requester_os: String,
}

impl RouteRequest {
    /// Build a route request from a [`JobRequest`].
    pub fn from_job(
        job: JobRequest,
        context: JobContext,
        sticky_peer: Option<NodeId>,
        requester_os: impl Into<String>,
    ) -> Self {
        Self {
            job,
            context,
            sticky_peer,
            requester_os: requester_os.into(),
        }
    }

    /// Model id being routed.
    pub fn model(&self) -> &str {
        &self.job.model
    }
}

/// Selected route.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteDecision {
    /// Chosen peer id.
    pub node_id: NodeId,
    /// Chosen peer listen address.
    pub listen_addr: String,
    /// Whether the choice is the local Slipstream node.
    pub is_local: bool,
    /// Total composite score (higher is better).
    pub score: i64,
    /// Breakdown for observability / debugging.
    pub breakdown: ScoreBreakdown,
}

/// Per-component score contributions (after gates pass).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScoreBreakdown {
    /// Local Slipstream bonus.
    pub local_bonus: i64,
    /// Backend / OS alignment.
    pub backend_fit: i64,
    /// Hardware headroom above the min gate.
    pub hardware_fit: i64,
    /// Estimated tok/s contribution.
    pub speed: i64,
    /// RTT penalty (subtracted).
    pub latency_penalty: i64,
    /// Price penalty (subtracted).
    pub price_penalty: i64,
    /// Reputation stub contribution.
    pub reputation: i64,
    /// Load penalty (subtracted).
    pub load_penalty: i64,
    /// Sticky coding-session bonus.
    pub sticky_bonus: i64,
}

impl ScoreBreakdown {
    /// Sum soft-score components.
    pub fn total(&self) -> i64 {
        self.local_bonus
            + self.backend_fit
            + self.hardware_fit
            + self.speed
            + self.reputation
            + self.sticky_bonus
            - self.latency_penalty
            - self.price_penalty
            - self.load_penalty
    }
}

#[cfg(test)]
pub(crate) mod test_util {
    use super::*;
    use p2p_core::{local_capability, BackendKind, Capability};

    /// Deterministic [`NodeId`] from a short label (padded to 32 bytes).
    pub fn nid(label: &str) -> NodeId {
        let mut bytes = [0u8; 32];
        let raw = label.as_bytes();
        let n = raw.len().min(32);
        bytes[..n].copy_from_slice(&raw[..n]);
        NodeId::from_bytes(&bytes)
    }

    pub fn peer(
        id: &str,
        local: bool,
        ram: u32,
        vram: u32,
        backend: BackendKind,
        os: &str,
        tok_s: f64,
        rtt: u32,
        price: u64,
        rep: u32,
        load: f64,
        models: &[&str],
    ) -> PeerSnapshot {
        let mut capability: Capability =
            local_capability(os, ram, vram, models.iter().map(|s| (*s).into()).collect());
        capability.backend = backend;
        capability.tok_s_estimate = tok_s;
        capability.price_credits_per_1k = price;
        PeerSnapshot {
            node_id: nid(id),
            listen_addr: format!("{id}.local:9"),
            capability,
            rtt_ms: rtt,
            reputation: rep,
            load,
            is_local: local,
        }
    }

    pub fn job(model: &str) -> JobRequest {
        JobRequest {
            job_id: "j1".into(),
            model: model.into(),
            system: String::new(),
            prompt: "hi".into(),
            max_tokens: 16,
        }
    }
}
