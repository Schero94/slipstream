//! Trust gate interface (v0.6 M2, plan §2 / §8 "trust gate before any read/decrypt").
//!
//! The forwarder does not own the trust store (that lives in `apps/p2p-node/src/trust.rs`
//! with pairing); it asks a [`PeerTrust`]:
//!
//! - at `ConnectionEstablished`: untrusted peers are disconnected at once, unless a
//!   pairing window is open (then they may stay, but only `/slipstream/pair/1` serves
//!   them — every other protocol re-checks trust per stream);
//! - per stream: `trusted_key` must return the peer's pinned static X25519 key, which
//!   also authenticates the SealedStream keys ([`p2p_crypto::StreamHandshake::finish_authenticated`]);
//! - on every change ([`PeerTrust::subscribe`]): connections of peers that lost trust
//!   are closed (after a best-effort [`PeerTrust::notify_revoked`], e.g. an `Unpair`
//!   message) and their active streams are aborted ([`until_untrusted`]).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use libp2p::PeerId;
use p2p_libp2p::paired::Control;
use tokio::sync::watch;

pub type BoxFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// A change counter shared by everything that can change trust (store reloads,
/// pairing window open/close). Cheap to clone.
#[derive(Clone)]
pub struct TrustEpoch(Arc<watch::Sender<u64>>);

impl Default for TrustEpoch {
    fn default() -> Self {
        Self(Arc::new(watch::channel(0).0))
    }
}

impl TrustEpoch {
    pub fn bump(&self) {
        self.0.send_modify(|v| *v = v.wrapping_add(1));
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.0.subscribe()
    }

    pub fn current(&self) -> u64 {
        *self.0.borrow()
    }
}

pub trait PeerTrust: Send + Sync + 'static {
    /// The static X25519 public key pinned for `peer` at pairing time, if trusted.
    fn trusted_key(&self, peer: &PeerId) -> Option<[u8; 32]>;

    /// Whether a pairing window is open right now (untrusted peers may connect, pair
    /// protocol only). Re-polled by the gate at least every [`GATE_TICK`].
    fn pairing_open(&self) -> bool {
        false
    }

    /// Notified whenever trust may have changed.
    fn subscribe(&self) -> watch::Receiver<u64>;

    /// Best-effort goodbye to a peer that just lost trust, sent before its connection is
    /// closed (bounded by the caller to [`NOTIFY_REVOKED_BUDGET`]).
    fn notify_revoked(&self, _peer: PeerId, _old_key: [u8; 32], _control: Control) -> BoxFuture {
        Box::pin(async {})
    }

    /// The static key a *removed* Mac had (a tombstone kept after revoke, M5). When such a
    /// Mac connects again — it was offline when it was removed — the gate answers with one
    /// `Unpair` ([`notify_revoked`](Self::notify_revoked), at most once per
    /// [`FORMER_UNPAIR_EVERY`] per peer) before it closes the connection, so the other Mac
    /// can show "removed you" instead of retrying forever.
    fn former_key(&self, _peer: &PeerId) -> Option<[u8; 32]> {
        None
    }
}

/// Rate limit of the `Unpair` answer to a removed Mac that keeps connecting.
pub const FORMER_UNPAIR_EVERY: std::time::Duration = std::time::Duration::from_secs(10);
/// Delay before that `Unpair` (the dialing side registers its pair-protocol acceptor
/// right after it starts dialing).
pub const FORMER_UNPAIR_DELAY: std::time::Duration = std::time::Duration::from_millis(250);

/// Gate re-evaluation tick (pairing-window expiry is noticed within this).
pub const GATE_TICK: std::time::Duration = std::time::Duration::from_millis(250);
/// Time a revoked peer's connection may stay open for the `Unpair` goodbye. Its active
/// streams are aborted immediately, not after this.
pub const NOTIFY_REVOKED_BUDGET: std::time::Duration = std::time::Duration::from_millis(400);

/// Resolves once `peer` no longer has exactly `key` pinned (revoked or re-paired).
pub async fn until_untrusted(trust: &dyn PeerTrust, peer: PeerId, key: [u8; 32]) {
    let mut rx = trust.subscribe();
    loop {
        if trust.trusted_key(&peer) != Some(key) {
            return;
        }
        if rx.changed().await.is_err() {
            // no more changes can happen
            std::future::pending::<()>().await;
        }
    }
}

/// In-memory trust (tests, benchmarks): PeerId → static X25519 public key.
#[derive(Default)]
pub struct StaticTrust {
    peers: Mutex<HashMap<PeerId, [u8; 32]>>,
    former: Mutex<HashMap<PeerId, [u8; 32]>>,
    open: std::sync::atomic::AtomicBool,
    epoch: TrustEpoch,
}

impl StaticTrust {
    pub fn new(peers: impl IntoIterator<Item = (PeerId, [u8; 32])>) -> Arc<Self> {
        let s = Self::default();
        s.peers.lock().unwrap().extend(peers);
        Arc::new(s)
    }

    pub fn insert(&self, peer: PeerId, key: [u8; 32]) {
        self.peers.lock().unwrap().insert(peer, key);
        self.epoch.bump();
    }

    pub fn remove(&self, peer: &PeerId) -> Option<[u8; 32]> {
        let r = self.peers.lock().unwrap().remove(peer);
        if let Some(k) = r {
            self.former.lock().unwrap().insert(*peer, k);
        }
        self.epoch.bump();
        r
    }

    pub fn set_pairing_open(&self, open: bool) {
        self.open.store(open, std::sync::atomic::Ordering::SeqCst);
        self.epoch.bump();
    }

    pub fn is_empty(&self) -> bool {
        self.peers.lock().unwrap().is_empty()
    }
}

impl PeerTrust for StaticTrust {
    fn trusted_key(&self, peer: &PeerId) -> Option<[u8; 32]> {
        self.peers.lock().unwrap().get(peer).copied()
    }

    fn pairing_open(&self) -> bool {
        self.open.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn subscribe(&self) -> watch::Receiver<u64> {
        self.epoch.subscribe()
    }

    fn former_key(&self, peer: &PeerId) -> Option<[u8; 32]> {
        self.former.lock().unwrap().get(peer).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn until_untrusted_fires_on_revoke_and_rekey() {
        let peer = PeerId::random();
        let trust = StaticTrust::new([(peer, [1u8; 32])]);
        let t2 = trust.clone();
        let waiter = tokio::spawn(async move { until_untrusted(&*t2, peer, [1u8; 32]).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        trust.insert(PeerId::random(), [3u8; 32]); // unrelated change
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        trust.insert(peer, [2u8; 32]); // re-paired with another key
        tokio::time::timeout(Duration::from_secs(1), waiter).await.unwrap().unwrap();
        // already untrusted → immediate
        trust.remove(&peer);
        tokio::time::timeout(Duration::from_millis(100), until_untrusted(&*trust, peer, [2u8; 32]))
            .await
            .unwrap();
    }
}
