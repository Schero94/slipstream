//! TCP peer session: connect/listen + typed [`NetMessage`] exchange.

use std::net::SocketAddr;
use std::time::Duration;

use p2p_security::ReplayCache;
use tokio::net::{TcpListener, TcpStream};

use crate::error::NetError;
use crate::frame::{read_frame, write_frame};
use crate::message::NetMessage;
use crate::replay::admit_encrypted_job;

/// Default dial budget — fail fast when a peer is down or firewalled.
/// Matches `p2p-engine` HTTP connect timeout so Cluster Probe / bootstrap stay snappy.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Bidirectional framed session over a single TCP stream.
pub struct PeerSession {
    stream: TcpStream,
    peer_addr: SocketAddr,
}

impl std::fmt::Debug for PeerSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerSession")
            .field("peer_addr", &self.peer_addr)
            .finish_non_exhaustive()
    }
}

impl PeerSession {
    pub fn from_stream(stream: TcpStream) -> Result<Self, NetError> {
        let peer_addr = stream.peer_addr()?;
        Ok(Self { stream, peer_addr })
    }

    pub fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }

    pub async fn send(&mut self, msg: &NetMessage) -> Result<(), NetError> {
        let body = msg.encode()?;
        write_frame(&mut self.stream, &body).await
    }

    /// Raw receive — no replay policy. Prefer [`Self::recv_with_replay`] for jobs.
    pub async fn recv(&mut self) -> Result<NetMessage, NetError> {
        let body = read_frame(&mut self.stream).await?;
        Ok(NetMessage::decode(&body)?)
    }

    /// Receive one frame; if it is [`NetMessage::EncryptedJob`], admit it into
    /// `cache` (fingerprint via `p2p_security::envelope_fingerprint`) or return
    /// [`NetError::Replay`] on duplicate `job_id`.
    pub async fn recv_with_replay(
        &mut self,
        cache: &mut ReplayCache,
    ) -> Result<NetMessage, NetError> {
        let msg = self.recv().await?;
        admit_encrypted_job(&msg, cache)?;
        Ok(msg)
    }

    /// Send Hello then expect Hello back (capability exchange).
    pub async fn exchange_hello(
        &mut self,
        ours: crate::message::CapabilityAdvert,
    ) -> Result<crate::message::CapabilityAdvert, NetError> {
        self.send(&NetMessage::Hello {
            capability: ours,
            auth: None,
        })
        .await?;
        match self.recv().await? {
            NetMessage::Hello { capability, .. } => Ok(capability),
            other => Err(NetError::protocol(format!(
                "expected hello during capability exchange, got {}",
                other.wire_type()
            ))),
        }
    }
}

/// Bind a TCP listener on `bind` (use `127.0.0.1:0` for ephemeral).
pub async fn listen(bind: SocketAddr) -> Result<(TcpListener, SocketAddr), NetError> {
    let listener = TcpListener::bind(bind).await?;
    let local = listener.local_addr()?;
    Ok((listener, local))
}

/// Accept one inbound session.
pub async fn accept(listener: &TcpListener) -> Result<PeerSession, NetError> {
    let (stream, _) = listener.accept().await?;
    PeerSession::from_stream(stream)
}

/// Dial `addr` with [`DEFAULT_CONNECT_TIMEOUT`].
pub async fn connect(addr: SocketAddr) -> Result<PeerSession, NetError> {
    connect_timeout(addr, DEFAULT_CONNECT_TIMEOUT).await
}

/// Dial `addr`, failing with [`NetError::Timeout`] if the TCP handshake exceeds `timeout`.
///
/// Critical for multi-node Probe / bootstrap: unbound `TcpStream::connect` can hang for
/// minutes on blackhole routes; Cluster UI and `p2p-node` need a bounded wait.
pub async fn connect_timeout(addr: SocketAddr, timeout: Duration) -> Result<PeerSession, NetError> {
    match tokio::time::timeout(timeout, TcpStream::connect(addr)).await {
        Ok(Ok(stream)) => PeerSession::from_stream(stream),
        Ok(Err(e)) => Err(NetError::Io(e)),
        Err(_) => Err(NetError::timeout(format!(
            "connect {addr} exceeded {timeout:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddr};

    #[tokio::test]
    async fn connect_timeout_returns_timeout_error() {
        // TEST-NET-3 (RFC 5737) — typically blackholed. Some hosts fail-fast with
        // Io (unreachable); either is fine so long as we do not hang unbound.
        let addr = SocketAddr::from((Ipv4Addr::new(203, 0, 113, 1), 9));
        let budget = Duration::from_millis(200);
        let started = std::time::Instant::now();
        let err = match connect_timeout(addr, budget).await {
            Ok(s) => panic!("blackhole dial must fail, got {s:?}"),
            Err(e) => e,
        };
        if err.is_timeout() {
            return;
        }
        assert!(
            matches!(err, NetError::Io(_)),
            "expected Timeout or fail-fast Io, got {err:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "Io fail-fast must not hang like an unbound dial"
        );
    }

    #[tokio::test]
    async fn connect_refused_is_io_not_timeout() {
        // Bind ephemeral then drop so nothing accepts — connect gets ConnectionRefused.
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local");
        drop(listener);
        let err = connect_timeout(addr, Duration::from_secs(2))
            .await
            .expect_err("refused dial must fail");
        assert!(!err.is_timeout(), "refused should be Io, got {err:?}");
        assert!(matches!(err, NetError::Io(_)), "expected Io, got {err:?}");
    }
}
