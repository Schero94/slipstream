//! Transport errors.

use std::io;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum NetError {
    #[error("io: {0}")]
    Io(#[from] io::Error),

    #[error("frame too large: {0} bytes (max {1}; raise MAX_FRAME_BYTES only with care)")]
    FrameTooLarge(usize, usize),

    /// Prefix length was 0 or above the soft max (see [`crate::MAX_FRAME_BYTES`]).
    #[error("invalid frame length {0} (allowed 1..={1} bytes)")]
    BadFrameLength(u32, usize),

    #[error("codec: {0}")]
    Codec(#[from] serde_json::Error),

    #[error("protocol: {0}")]
    Protocol(String),

    /// Duplicate EncryptedJob rejected by `p2p_security::ReplayCache`.
    #[error("replay: {0}")]
    Replay(String),

    /// Dial or handshake exceeded the configured wall-clock budget.
    #[error("timeout: {0}")]
    Timeout(String),
}

impl NetError {
    pub fn protocol(msg: impl Into<String>) -> Self {
        Self::Protocol(msg.into())
    }

    pub fn timeout(msg: impl Into<String>) -> Self {
        Self::Timeout(msg.into())
    }

    pub fn is_replay(&self) -> bool {
        matches!(self, Self::Replay(_))
    }

    pub fn is_timeout(&self) -> bool {
        matches!(self, Self::Timeout(_))
    }
}
