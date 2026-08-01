//! Peer discovery: bootstrap list (primary) + optional mDNS.

use std::net::SocketAddr;
use std::str::FromStr;

use crate::error::NetError;

/// A discovered or configured peer endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PeerAddr {
    pub label: Option<String>,
    pub addr: SocketAddr,
}

impl PeerAddr {
    pub fn new(addr: SocketAddr) -> Self {
        Self { label: None, addr }
    }

    pub fn with_label(addr: SocketAddr, label: impl Into<String>) -> Self {
        Self {
            label: Some(label.into()),
            addr,
        }
    }
}

impl FromStr for PeerAddr {
    type Err = NetError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // "label@host:port" or "host:port"
        if let Some((label, rest)) = s.split_once('@') {
            let addr: SocketAddr = rest
                .parse()
                .map_err(|e| NetError::protocol(format!("bad peer addr '{s}': {e}")))?;
            Ok(Self::with_label(addr, label))
        } else {
            let addr: SocketAddr = s
                .parse()
                .map_err(|e| NetError::protocol(format!("bad peer addr '{s}': {e}")))?;
            Ok(Self::new(addr))
        }
    }
}

/// Explicit bootstrap peers — enough for LAN/dev and deterministic tests.
#[derive(Debug, Clone, Default)]
pub struct BootstrapList {
    peers: Vec<PeerAddr>,
}

impl BootstrapList {
    pub fn new(peers: impl IntoIterator<Item = PeerAddr>) -> Self {
        Self {
            peers: peers.into_iter().collect(),
        }
    }

    pub fn from_addrs(addrs: impl IntoIterator<Item = SocketAddr>) -> Self {
        Self::new(addrs.into_iter().map(PeerAddr::new))
    }

    /// Parse a comma-separated list: `a@127.0.0.1:9001,127.0.0.1:9002`.
    pub fn parse_csv(csv: &str) -> Result<Self, NetError> {
        let mut peers = Vec::new();
        for part in csv.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            peers.push(part.parse()?);
        }
        Ok(Self::new(peers))
    }

    pub fn peers(&self) -> &[PeerAddr] {
        &self.peers
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    pub fn push(&mut self, peer: PeerAddr) {
        self.peers.push(peer);
    }
}

/// Optional mDNS helpers (feature `mdns`).
///
/// Service type: `_slipstream-p2p._tcp.local.`
#[cfg(feature = "mdns")]
pub mod mdns {
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::time::Duration;

    use mdns_sd::{ServiceDaemon, ServiceInfo};

    use super::PeerAddr;
    use crate::error::NetError;

    pub const SERVICE_TYPE: &str = "_slipstream-p2p._tcp.local.";

    /// Announce this node on the local link via mDNS.
    pub fn announce(instance: &str, port: u16, host_ipv4: &str) -> Result<ServiceDaemon, NetError> {
        let daemon = ServiceDaemon::new()
            .map_err(|e| NetError::protocol(format!("mdns daemon: {e}")))?;
        let props = HashMap::new();
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            instance,
            &format!("{host_ipv4}.local."),
            host_ipv4,
            port,
            props,
        )
        .map_err(|e| NetError::protocol(format!("mdns service info: {e}")))?;
        daemon
            .register(info)
            .map_err(|e| NetError::protocol(format!("mdns register: {e}")))?;
        Ok(daemon)
    }

    /// Browse briefly for peers; returns whatever answers within `timeout`.
    pub fn browse(timeout: Duration) -> Result<Vec<PeerAddr>, NetError> {
        let daemon = ServiceDaemon::new()
            .map_err(|e| NetError::protocol(format!("mdns daemon: {e}")))?;
        let receiver = daemon
            .browse(SERVICE_TYPE)
            .map_err(|e| NetError::protocol(format!("mdns browse: {e}")))?;
        let deadline = std::time::Instant::now() + timeout;
        let mut found = Vec::new();
        while std::time::Instant::now() < deadline {
            let remain = deadline.saturating_duration_since(std::time::Instant::now());
            match receiver.recv_timeout(remain.min(Duration::from_millis(200))) {
                Ok(event) => {
                    if let mdns_sd::ServiceEvent::ServiceResolved(info) = event {
                        let port = info.get_port();
                        for ip in info.get_addresses() {
                            if let Ok(addr) = format!("{ip}:{port}").parse::<SocketAddr>() {
                                found.push(PeerAddr::with_label(addr, info.get_fullname()));
                            }
                        }
                    }
                }
                Err(_) => break,
            }
        }
        let _ = daemon.shutdown();
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};

    #[test]
    fn parse_csv_with_labels() {
        let list = BootstrapList::parse_csv("alice@127.0.0.1:9001, 127.0.0.1:9002").unwrap();
        assert_eq!(list.peers().len(), 2);
        assert_eq!(list.peers()[0].label.as_deref(), Some("alice"));
        assert_eq!(
            list.peers()[0].addr,
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9001))
        );
        assert!(list.peers()[1].label.is_none());
    }

    #[test]
    fn empty_csv() {
        assert!(BootstrapList::parse_csv("  , ").unwrap().is_empty());
    }

    #[test]
    fn bad_peer_addr_is_actionable() {
        let err = BootstrapList::parse_csv("not-a-peer").unwrap_err();
        let s = err.to_string();
        assert!(s.contains("bad peer addr"), "display={s}");
        assert!(s.contains("not-a-peer"), "display={s}");
    }
}
