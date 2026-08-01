//! Anti-Sybil MVP: bootstrap peer allowlist (TM-003).
//!
//! Later: stake / proof-of-resource. MVP treats unknown pubkeys as untrusted
//! for advertise/earn even if they speak the protocol.

use std::collections::HashSet;

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SybilError {
    #[error("peer not on bootstrap allowlist")]
    NotAllowlisted,
}

/// Set of node public-key hex ids permitted to join / earn.
#[derive(Debug, Clone, Default)]
pub struct Allowlist {
    peers: HashSet<String>,
}

impl Allowlist {
    pub fn new(peer_ids: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            peers: peer_ids.into_iter().map(Into::into).collect(),
        }
    }

    pub fn insert(&mut self, peer_id_hex: impl Into<String>) {
        self.peers.insert(peer_id_hex.into());
    }

    pub fn contains(&self, peer_id_hex: &str) -> bool {
        self.peers.contains(peer_id_hex)
    }

    /// Reject advertise/earn from unknown identities (Sybil gate).
    pub fn admit_advertiser(&self, peer_id_hex: &str) -> Result<(), SybilError> {
        if self.contains(peer_id_hex) {
            Ok(())
        } else {
            Err(SybilError::NotAllowlisted)
        }
    }
}
