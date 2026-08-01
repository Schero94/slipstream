//! Local in-process node API (no network). Networking lives in `p2p-net` / `p2p-sim`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::capability::Capability;
use crate::engine::{InferenceEngine, MockEngine};
use crate::identity::{NodeId, NodeIdentity};
use crate::job::{JobRequest, JobResult};

/// Snapshot of a known peer (routing details filled by `p2p-router` / `p2p-net`).
#[derive(Debug, Clone, PartialEq)]
pub struct PeerRecord {
    pub id: NodeId,
    pub listen_addr: String,
    pub capability: Capability,
}

/// Configuration for a local in-process node.
#[derive(Debug, Clone)]
pub struct LocalNodeConfig {
    pub identity: NodeIdentity,
    pub capability: Capability,
    pub listen_addr: String,
}

/// In-process node: holds identity/capability, peer table, and an engine.
///
/// Jobs submitted via [`LocalNode::submit_local`] run on this node's engine.
/// Cross-node routing / encryption is owned by sibling crates.
pub struct LocalNode {
    config: LocalNodeConfig,
    engine: Arc<dyn InferenceEngine>,
    peers: Mutex<HashMap<String, PeerRecord>>,
    /// Jobs completed locally (job_id → result). Useful for tests / CLI stubs.
    completed: Mutex<HashMap<String, JobResult>>,
}

impl LocalNode {
    pub fn new(config: LocalNodeConfig, engine: Arc<dyn InferenceEngine>) -> Self {
        Self {
            config,
            engine,
            peers: Mutex::new(HashMap::new()),
            completed: Mutex::new(HashMap::new()),
        }
    }

    /// Convenience: mock engine + generated identity.
    pub fn with_mock(capability: Capability, listen_addr: impl Into<String>) -> Self {
        Self::new(
            LocalNodeConfig {
                identity: NodeIdentity::generate(),
                capability,
                listen_addr: listen_addr.into(),
            },
            Arc::new(MockEngine),
        )
    }

    pub fn id(&self) -> &NodeId {
        self.config.identity.id()
    }

    pub fn identity(&self) -> &NodeIdentity {
        &self.config.identity
    }

    pub fn capability(&self) -> &Capability {
        &self.config.capability
    }

    pub fn listen_addr(&self) -> &str {
        &self.config.listen_addr
    }

    pub fn upsert_peer(&self, peer: PeerRecord) {
        let mut map = self.peers.lock().expect("peers lock");
        map.insert(peer.id.as_hex().to_string(), peer);
    }

    pub fn peers(&self) -> Vec<PeerRecord> {
        self.peers.lock().expect("peers lock").values().cloned().collect()
    }

    pub fn peer(&self, id: &NodeId) -> Option<PeerRecord> {
        self.peers
            .lock()
            .expect("peers lock")
            .get(id.as_hex())
            .cloned()
    }

    /// Run a job on the local engine. Does not touch the network.
    pub fn submit_local(&self, job: JobRequest) -> JobResult {
        let result = if !self.config.capability.meets_min_hardware() {
            JobResult::failure(&job.job_id, "local node below min hardware")
        } else if !self.config.capability.supports_model(&job.model) {
            JobResult::failure(
                &job.job_id,
                format!("local node does not serve model {}", job.model),
            )
        } else {
            self.engine.infer(&job)
        };
        self.completed
            .lock()
            .expect("completed lock")
            .insert(result.job_id.clone(), result.clone());
        result
    }

    pub fn completed_job(&self, job_id: &str) -> Option<JobResult> {
        self.completed
            .lock()
            .expect("completed lock")
            .get(job_id)
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::local_capability;

    #[test]
    fn local_node_mock_infers() {
        let cap = local_capability("linux", 32, 0, vec!["mock".into()]);
        let node = LocalNode::with_mock(cap, "127.0.0.1:0");
        let result = node.submit_local(JobRequest {
            job_id: "job-a".into(),
            model: "mock".into(),
            system: String::new(),
            prompt: "ping".into(),
            max_tokens: 2,
        });
        assert!(result.ok);
        assert_eq!(result.tokens, 2);
        assert_eq!(node.completed_job("job-a").unwrap().text, result.text);
    }

    #[test]
    fn local_node_rejects_unsupported_model() {
        let cap = local_capability("macos", 36, 0, vec!["a".into()]);
        let node = LocalNode::with_mock(cap, "inproc");
        let result = node.submit_local(JobRequest {
            job_id: "j".into(),
            model: "b".into(),
            system: String::new(),
            prompt: "x".into(),
            max_tokens: 1,
        });
        assert!(!result.ok);
    }

    #[test]
    fn peer_table_upsert() {
        let cap = local_capability("linux", 32, 0, vec!["m".into()]);
        let node = LocalNode::with_mock(cap.clone(), "a");
        let peer_id = NodeIdentity::from_secret_bytes([2u8; 32]).id().clone();
        node.upsert_peer(PeerRecord {
            id: peer_id.clone(),
            listen_addr: "127.0.0.1:9".into(),
            capability: cap,
        });
        assert_eq!(node.peers().len(), 1);
        assert_eq!(node.peer(&peer_id).unwrap().listen_addr, "127.0.0.1:9");
    }
}
