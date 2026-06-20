//! Bounded in-memory dedup cache for alert IDs (ADR-013).
//!
//! JetStream provides at-least-once delivery, and ADR-013 introduces the
//! possibility of the same alert arriving via both the webhook and NATS
//! during cutover or replay. This cache absorbs the high-frequency case
//! so the orchestrator avoids a DB roundtrip per delivery.
//!
//! Semantics: bounded FIFO (eviction by insertion order, not last-use).
//! For the duplicate-detection use case this is equivalent to LRU but
//! cheaper — we never need to promote an already-seen entry, only
//! recognize it.
//!
//! The durable workflow store (`workflows.alert_id`) remains the
//! authoritative dedup record; this cache exists only to keep the
//! steady-state hot path off the database.

use std::collections::{HashMap, VecDeque};

/// Bounded FIFO cache that records which alert IDs have been seen
/// recently. On insertion, evicts the oldest entry when at capacity.
///
/// Thread-safety: wrap in a `std::sync::Mutex` for shared use. The
/// `check_and_insert` operation is constant-time and never blocks, so
/// holding the mutex across the call is safe in async code.
#[derive(Debug)]
pub struct AlertDedupCache {
    capacity: usize,
    seen: HashMap<String, ()>,
    order: VecDeque<String>,
}

impl AlertDedupCache {
    /// Construct a cache with the given capacity.
    ///
    /// `capacity` of 0 is treated as 1 — a degenerate cache that still
    /// detects back-to-back duplicates. This keeps callers free of
    /// boundary handling without producing a permanently empty cache.
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            capacity,
            seen: HashMap::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
        }
    }

    /// Returns `true` if `alert_id` was newly inserted (first time seen),
    /// `false` if it was already present (duplicate).
    ///
    /// Constant-time on average. Evicts the oldest entry when at
    /// capacity and the entry is new.
    pub fn check_and_insert(&mut self, alert_id: &str) -> bool {
        if self.seen.contains_key(alert_id) {
            return false;
        }
        if self.seen.len() >= self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.seen.remove(&oldest);
            }
        }
        self.seen.insert(alert_id.to_string(), ());
        self.order.push_back(alert_id.to_string());
        true
    }

    /// Number of entries currently held.
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    /// Cache is empty?
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }

    /// Configured capacity (post-`max(1)` adjustment).
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_entry_returns_true() {
        let mut cache = AlertDedupCache::new(10);
        assert!(cache.check_and_insert("a"));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn duplicate_returns_false() {
        let mut cache = AlertDedupCache::new(10);
        assert!(cache.check_and_insert("a"));
        assert!(!cache.check_and_insert("a"));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn eviction_at_capacity_keeps_newest() {
        let mut cache = AlertDedupCache::new(3);
        assert!(cache.check_and_insert("a"));
        assert!(cache.check_and_insert("b"));
        assert!(cache.check_and_insert("c"));
        // "a" should be evicted
        assert!(cache.check_and_insert("d"));
        assert_eq!(cache.len(), 3);
        // "a" can be re-inserted (it was forgotten)
        assert!(cache.check_and_insert("a"));
        // "b", "c", "d" should still be remembered
        assert!(!cache.check_and_insert("c"));
        assert!(!cache.check_and_insert("d"));
    }

    #[test]
    fn zero_capacity_is_promoted_to_one() {
        let mut cache = AlertDedupCache::new(0);
        assert_eq!(cache.capacity(), 1);
        assert!(cache.check_and_insert("a"));
        // Back-to-back duplicate is still caught
        assert!(!cache.check_and_insert("a"));
    }

    #[test]
    fn many_distinct_alerts_stay_bounded() {
        let mut cache = AlertDedupCache::new(50);
        for i in 0..1000 {
            cache.check_and_insert(&format!("alert-{i}"));
        }
        assert_eq!(cache.len(), 50);
        assert!(cache.seen.len() == cache.order.len());
    }
}
