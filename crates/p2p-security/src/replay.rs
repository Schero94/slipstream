//! Replay protection via job_id cache (TM-004).
//!
//! MVP: reject duplicate `job_id` within a sliding window. Optional envelope
//! fingerprint (from `p2p_crypto::SealedEnvelope` fields) can be recorded
//! alongside for defense-in-depth.
//!
//! Note: AEAD (`seal`/`open`) already rejects wrong-key and tamper; it does
//! **not** stop honest re-submission of a captured valid envelope — that is
//! this module's job.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use thiserror::Error;

/// Stable fingerprint of a sealed envelope wire blob (eph pub + ciphertext hex).
///
/// Aligns with `p2p_crypto::SealedEnvelope` field names without depending on
/// that crate at library compile time (tests may pass the same strings).
pub fn envelope_fingerprint(eph_pub_hex: &str, ciphertext_hex: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(eph_pub_hex.as_bytes());
    hasher.update(b"|");
    hasher.update(ciphertext_hex.as_bytes());
    hex::encode(hasher.finalize())
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ReplayError {
    #[error("duplicate job_id (replay)")]
    DuplicateJobId,
}

#[derive(Debug)]
struct Entry {
    seen_at: Instant,
    /// Optional sealed-envelope fingerprint for defense-in-depth correlators.
    #[allow(dead_code)]
    envelope_fp: Option<String>,
}

/// In-memory replay window for accepted job identifiers.
#[derive(Debug)]
pub struct ReplayCache {
    ttl: Duration,
    max_entries: usize,
    entries: HashMap<String, Entry>,
    order: VecDeque<(String, Instant)>,
}

impl ReplayCache {
    pub fn new(ttl: Duration) -> Self {
        Self::with_capacity(ttl, 65_536)
    }

    pub fn with_capacity(ttl: Duration, max_entries: usize) -> Self {
        Self {
            ttl,
            max_entries: max_entries.max(1),
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    pub fn with_default_ttl() -> Self {
        Self::new(Duration::from_secs(24 * 60 * 60))
    }

    fn purge_expired(&mut self, now: Instant) {
        while let Some((job_id, seen_at)) = self.order.front() {
            if now.duration_since(*seen_at) <= self.ttl {
                break;
            }
            let job_id = job_id.clone();
            let seen_at = *seen_at;
            self.order.pop_front();
            if self
                .entries
                .get(&job_id)
                .is_some_and(|entry| entry.seen_at == seen_at)
            {
                self.entries.remove(&job_id);
            }
        }
    }

    fn evict_to_fit(&mut self) {
        while self.entries.len() >= self.max_entries {
            let Some((job_id, seen_at)) = self.order.pop_front() else {
                break;
            };
            if self
                .entries
                .get(&job_id)
                .is_some_and(|entry| entry.seen_at == seen_at)
            {
                self.entries.remove(&job_id);
            }
        }
    }

    /// Accept a fresh job_id, or reject if already seen inside the TTL window.
    pub fn accept(&mut self, job_id: &str, envelope_fp: Option<&str>) -> Result<(), ReplayError> {
        let now = Instant::now();
        self.purge_expired(now);
        if self.entries.contains_key(job_id) {
            return Err(ReplayError::DuplicateJobId);
        }
        self.evict_to_fit();
        self.entries.insert(
            job_id.to_string(),
            Entry {
                seen_at: now,
                envelope_fp: envelope_fp.map(str::to_string),
            },
        );
        self.order.push_back((job_id.to_string(), now));
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Test helper: whether a job_id is currently remembered.
    pub fn contains(&self, job_id: &str) -> bool {
        self.entries.contains_key(job_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_deterministic_and_order_sensitive() {
        let a = envelope_fingerprint("pub", "ct");
        let b = envelope_fingerprint("pub", "ct");
        let c = envelope_fingerprint("ct", "pub");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn accept_rejects_duplicate_job_id() {
        let mut cache = ReplayCache::new(Duration::from_secs(60));
        assert!(cache.is_empty());
        cache.accept("job-1", Some("fp")).unwrap();
        assert_eq!(cache.len(), 1);
        assert!(cache.contains("job-1"));
        assert_eq!(
            cache.accept("job-1", None),
            Err(ReplayError::DuplicateJobId)
        );
    }

    #[test]
    fn bounded_cache_evicts_oldest_entry() {
        let mut cache = ReplayCache::with_capacity(Duration::from_secs(60), 2);
        cache.accept("job-1", None).unwrap();
        cache.accept("job-2", None).unwrap();
        cache.accept("job-3", None).unwrap();

        assert_eq!(cache.len(), 2);
        assert!(!cache.contains("job-1"));
        assert!(cache.contains("job-2"));
        assert!(cache.contains("job-3"));
    }
}
