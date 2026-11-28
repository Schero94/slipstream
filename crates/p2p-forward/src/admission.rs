//! Paired-peer admission (plan §4): per peer at most 1 inference request in flight and 2
//! queued; the next one is refused with 503 + `Retry-After: 5`. llama-server runs with
//! `--parallel 1`, so a deeper queue would only hide latency from the editor.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use libp2p::PeerId;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionLimits {
    pub in_flight_per_peer: usize,
    pub queued_per_peer: usize,
}

impl Default for AdmissionLimits {
    fn default() -> Self {
        Self {
            in_flight_per_peer: 1,
            queued_per_peer: 2,
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
        }
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
