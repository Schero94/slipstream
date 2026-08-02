//! # p2p-net
//!
//! Minimal LAN/dev transport for Slipstream token-torrent inference.
//!
//! - **Transport:** tokio TCP + u32-BE length-prefixed JSON frames
//! - **Discovery:** explicit bootstrap peer list; optional mDNS behind `mdns` feature
//! - **Replay:** [`PeerSession::recv_with_replay`] + [`admit_encrypted_job`] via `p2p-security`
//! - **Not BitTorrent / not libp2p** — see `docs/p2p/TRANSPORT.md`
//!
//! Wire messages are transport-owned for MVP. Higher crates (`p2p-core`, `p2p-crypto`)
//! own semantic job/capability types; map at the boundary when those stabilize
//! (`TODO(core)`).

#![forbid(unsafe_code)]

pub mod discovery;
pub mod error;
pub mod frame;
pub mod message;
pub mod replay;
pub mod session;

pub use discovery::{BootstrapList, PeerAddr};
pub use error::NetError;
pub use frame::{read_frame, write_frame, MAX_FRAME_BYTES};
pub use message::{CapabilityAdvert, HelloAuth, NetMessage};
pub use replay::{admit_encrypted_job, encrypted_job_fingerprint};
pub use session::{accept, connect, connect_timeout, listen, PeerSession, DEFAULT_CONNECT_TIMEOUT};
