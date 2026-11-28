//! Paired-peer admission (plan §4): per peer at most 1 inference request in flight and 2
//! queued; the next one is refused with 503 + `Retry-After: 5`. llama-server runs with
//! `--parallel 1`, so a deeper queue would only hide latency from the editor.
//!
//! Light routes (models, health, tokenize/detokenize/apply-template, metrics, props,
//! slots) do not take the model slot, but are capped too (M1 security review): at most
//! [`AdmissionLimits::light_per_peer`] concurrent per peer and
//! [`AdmissionLimits::light_global`] over all peers; a request over the cap waits up to
//! [`AdmissionLimits::light_wait`], then gets 503.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use libp2p::PeerId;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionLimits {
    pub in_flight_per_peer: usize,
    pub queued_per_peer: usize,
    /// Concurrent light-route requests per peer.
    pub light_per_peer: usize,
    /// Concurrent light-route requests over all peers.
    pub light_global: usize,
    /// How long a light request over the cap waits before 503.
    pub light_wait: Duration,
}

impl Default for AdmissionLimits {
    fn default() -> Self {
        Self {
            in_flight_per_peer: 1,
            queued_per_peer: 2,
            light_per_peer: 4,
            light_global: 16,
            light_wait: Duration::from_secs(2),
        }
    }
}

struct PeerSlots {
    running: Arc<Semaphore>,
    queued: usize,
}

#[derive(Clone)]
pub struct Admission {
    limits: AdmissionLimits,
    peers: Arc<Mutex<HashMap<PeerId, PeerSlots>>>,
    light_peers: Arc<Mutex<HashMap<PeerId, Arc<Semaphore>>>>,
    light_global: Arc<Semaphore>,
}

/// Held while a light request runs.
pub struct LightPermit {
    _peer: OwnedSemaphorePermit,
    _global: OwnedSemaphorePermit,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Busy;

/// Held while a request runs; dropping it admits the next queued request.
pub struct Permit {
    _permit: OwnedSemaphorePermit,
}

struct QueueGuard {
    peers: Arc<Mutex<HashMap<PeerId, PeerSlots>>>,
    peer: PeerId,
}

impl Drop for QueueGuard {
    fn drop(&mut self) {
        if let Some(slots) = self.peers.lock().unwrap().get_mut(&self.peer) {
            slots.queued = slots.queued.saturating_sub(1);
        }
    }
}

impl Admission {
    pub fn new(limits: AdmissionLimits) -> Self {
        Self {
            limits,
            peers: Arc::default(),
            light_peers: Arc::default(),
            light_global: Arc::new(Semaphore::new(limits.light_global.max(1))),
        }
    }

    /// Admit a light-route request: per-peer and global cap, waiting at most
    /// `light_wait` for a place.
    pub async fn acquire_light(&self, peer: PeerId) -> Result<LightPermit, Busy> {
        let per_peer = self
            .light_peers
            .lock()
            .unwrap()
            .entry(peer)
            .or_insert_with(|| Arc::new(Semaphore::new(self.limits.light_per_peer.max(1))))
            .clone();
        let global = self.light_global.clone();
        let both = async move {
            let p = per_peer.acquire_owned().await.map_err(|_| Busy)?;
            let g = global.acquire_owned().await.map_err(|_| Busy)?;
            Ok(LightPermit {
                _peer: p,
                _global: g,
            })
        };
        tokio::time::timeout(self.limits.light_wait, both)
            .await
            .map_err(|_| Busy)?
    }

    /// Admit now, queue (waiting for the running request to finish), or refuse.
    /// Dropping the returned future while queued frees the queue place.
    pub async fn acquire(&self, peer: PeerId) -> Result<Permit, Busy> {
        let (running, guard) = {
            let mut peers = self.peers.lock().unwrap();
            let slots = peers.entry(peer).or_insert_with(|| PeerSlots {
                running: Arc::new(Semaphore::new(self.limits.in_flight_per_peer.max(1))),
                queued: 0,
            });
            if let Ok(permit) = slots.running.clone().try_acquire_owned() {
                return Ok(Permit { _permit: permit });
            }
            if slots.queued >= self.limits.queued_per_peer {
                return Err(Busy);
            }
            slots.queued += 1;
            (
                slots.running.clone(),
                QueueGuard {
                    peers: self.peers.clone(),
                    peer,
                },
            )
        };
        let permit = running.acquire_owned().await.map_err(|_| Busy)?;
        drop(guard);
        Ok(Permit { _permit: permit })
    }

    /// (requests running, requests queued) over all peers.
    pub fn load(&self) -> (usize, usize) {
        let peers = self.peers.lock().unwrap();
        let running = peers
            .values()
            .map(|s| self.limits.in_flight_per_peer.max(1) - s.running.available_permits())
            .sum();
        let queued = peers.values().map(|s| s.queued).sum();
        (running, queued)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn one_in_flight_two_queued_then_busy_per_peer() {
        let adm = Admission::new(AdmissionLimits::default());
        let a = PeerId::random();
        let b = PeerId::random();
        let first = adm.acquire(a).await.unwrap();
        let q1 = tokio::spawn({
            let adm = adm.clone();
            async move { adm.acquire(a).await.map(|_| ()) }
        });
        let q2 = tokio::spawn({
            let adm = adm.clone();
            async move { adm.acquire(a).await.map(|_| ()) }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(adm.load(), (1, 2));
        assert_eq!(adm.acquire(a).await.err(), Some(Busy));
        // another peer is independent
        let other = adm.acquire(b).await.unwrap();
        drop(other);
        drop(first);
        q1.await.unwrap().unwrap();
        q2.await.unwrap().unwrap();
        assert_eq!(adm.load(), (0, 0));
    }

    /// M1 review finding 3: light routes are capped per peer (4) and globally.
    #[tokio::test]
    async fn light_routes_capped_per_peer_and_globally() {
        let adm = Admission::new(AdmissionLimits {
            light_global: 6,
            light_wait: Duration::from_millis(50),
            ..AdmissionLimits::default()
        });
        let a = PeerId::random();
        let b = PeerId::random();
        let held: Vec<_> = futures::future::join_all((0..4).map(|_| adm.acquire_light(a)))
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect();
        // 5th concurrent light request of the same peer: waits, then Busy
        let t = std::time::Instant::now();
        assert!(adm.acquire_light(a).await.is_err());
        assert!(t.elapsed() >= Duration::from_millis(45));
        // another peer still gets in, up to the global cap (6)
        let b1 = adm.acquire_light(b).await.unwrap();
        let b2 = adm.acquire_light(b).await.unwrap();
        assert!(adm.acquire_light(b).await.is_err(), "global cap");
        drop(b1);
        let _b3 = adm.acquire_light(b).await.unwrap();
        // a waiter is admitted when a place frees up within light_wait
        let waiter = tokio::spawn({
            let adm = adm.clone();
            async move { adm.acquire_light(a).await.map(|_| ()) }
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        drop(b2);
        drop(held);
        waiter.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn cancelled_waiter_frees_its_queue_place() {
        let adm = Admission::new(AdmissionLimits::default());
        let a = PeerId::random();
        let _first = adm.acquire(a).await.unwrap();
        for _ in 0..5 {
            let r = tokio::time::timeout(Duration::from_millis(10), adm.acquire(a)).await;
            assert!(r.is_err(), "waits while the slot is taken");
        }
        assert_eq!(adm.load(), (1, 0));
    }
}
