//! Safe operating-mode and resource-policy defaults for the node daemon.

use std::fmt;
use std::net::SocketAddr;
use std::str::FromStr;

use p2p_security::DosLimits;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeMode {
    /// Local inference only. The listener must remain on loopback.
    Local,
    /// Manually configured LAN/private peers.
    Private,
    /// Permissionless public mesh. Donating capacity still requires opt-in.
    Community,
}

impl Default for NodeMode {
    fn default() -> Self {
        Self::Local
    }
}

impl fmt::Display for NodeMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Local => "local",
            Self::Private => "private",
            Self::Community => "community",
        })
    }
}

impl FromStr for NodeMode {
    type Err = PolicyError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "local" => Ok(Self::Local),
            "private" => Ok(Self::Private),
            "community" => Ok(Self::Community),
            other => Err(PolicyError::UnknownMode(other.to_string())),
        }
    }
}

impl NodeMode {
    pub fn validate_listen(self, listen: SocketAddr) -> Result<(), PolicyError> {
        if self == Self::Local && !listen.ip().is_loopback() {
            return Err(PolicyError::LocalRequiresLoopback(listen));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NodePolicy {
    pub mode: NodeMode,
    /// Public capacity donation is always explicit, including in community mode.
    pub donate_capacity: bool,
    pub limits: DosLimits,
    pub max_queue_jobs: u32,
    pub max_replay_entries: usize,
    pub peer_window_secs: u64,
}

impl Default for NodePolicy {
    fn default() -> Self {
        Self::for_mode(NodeMode::Local)
    }
}

impl NodePolicy {
    pub fn for_mode(mode: NodeMode) -> Self {
        Self {
            mode,
            donate_capacity: false,
            limits: DosLimits::default(),
            max_queue_jobs: 16,
            max_replay_entries: 65_536,
            peer_window_secs: 60,
        }
    }

    pub fn validate(&self) -> Result<(), PolicyError> {
        if self.donate_capacity && self.mode != NodeMode::Community {
            return Err(PolicyError::DonationRequiresCommunity);
        }
        if self.max_queue_jobs == 0 {
            return Err(PolicyError::ZeroLimit("max_queue_jobs"));
        }
        if self.max_replay_entries == 0 {
            return Err(PolicyError::ZeroLimit("max_replay_entries"));
        }
        if self.peer_window_secs == 0 {
            return Err(PolicyError::ZeroLimit("peer_window_secs"));
        }
        Ok(())
    }

    pub fn validate_for_listen(&self, listen: SocketAddr) -> Result<(), PolicyError> {
        self.validate()?;
        self.mode.validate_listen(listen)
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PolicyError {
    #[error("unknown node mode '{0}' (expected local, private, or community)")]
    UnknownMode(String),
    #[error("local mode requires a loopback listen address, got {0}")]
    LocalRequiresLoopback(SocketAddr),
    #[error("public capacity donation requires community mode")]
    DonationRequiresCommunity,
    #[error("{0} must be greater than zero")]
    ZeroLimit(&'static str),
}
