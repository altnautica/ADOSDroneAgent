//! A bounded single-use nonce cache for short-lived HMAC tickets.
//!
//! An HMAC ticket on its own is a bearer token: anyone who observes one can
//! present it again until it expires. The relay lane is a broadcast every fleet
//! member hears, and a WebSocket ticket travels in a subprotocol header that
//! can surface in logs, so both need "this exact ticket has already been
//! spent" as well as "this ticket is authentic".
//!
//! The cache remembers each admitted nonce until the ticket that carried it
//! can no longer verify anyway, and never for less than a fixed retention. It
//! is bounded: past its capacity the oldest entry is evicted. Only tickets that
//! already passed their HMAC check reach the cache, so filling it requires the
//! signing key, and an eviction cannot be forced by a stranger.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;

/// Remembers spent nonces. Cheap to share behind a `&'static` or an `Arc`.
pub struct NonceCache {
    capacity: usize,
    retention_seconds: i64,
    inner: Mutex<Inner>,
}

struct Inner {
    /// nonce → unix second after which it may be forgotten. A `BTreeMap` so
    /// the cache can be built in a `const` context and live in a `static`.
    seen: BTreeMap<String, i64>,
    /// Insertion order, for oldest-first eviction. May hold nonces already
    /// purged from `seen`; those are skipped when evicting.
    order: VecDeque<String>,
}

impl NonceCache {
    /// A cache holding at most `capacity` nonces, each for at least
    /// `retention_seconds`.
    pub const fn new(capacity: usize, retention_seconds: i64) -> Self {
        Self {
            capacity,
            retention_seconds,
            inner: Mutex::new(Inner {
                seen: BTreeMap::new(),
                order: VecDeque::new(),
            }),
        }
    }

    /// Record `nonce` as spent at `now` (unix seconds). Returns `true` when it
    /// was fresh and is now recorded, `false` when it was already spent.
    ///
    /// `valid_until` is the last unix second at which the ticket carrying the
    /// nonce could still verify; the entry is kept at least that long, so a
    /// ticket can never outlive its own replay record.
    pub fn admit(&self, nonce: &str, now: i64, valid_until: i64) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(forget_at) = inner.seen.remove(nonce) {
            if forget_at > now {
                inner.seen.insert(nonce.to_owned(), forget_at);
                return false;
            }
            // Expired: forget the old record entirely before re-admitting.
            inner.order.retain(|n| n != nonce);
        }
        let forget_at = now
            .saturating_add(self.retention_seconds)
            .max(valid_until.saturating_add(1));
        if inner.seen.len() >= self.capacity {
            inner.seen.retain(|_, f| *f > now);
            let Inner { seen, order } = &mut *inner;
            order.retain(|n| seen.contains_key(n));
        }
        while inner.seen.len() >= self.capacity {
            let Some(oldest) = inner.order.pop_front() else {
                break;
            };
            inner.seen.remove(&oldest);
        }
        inner.seen.insert(nonce.to_owned(), forget_at);
        inner.order.push_back(nonce.to_owned());
        true
    }

    /// How many nonces are currently remembered.
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .seen
            .len()
    }

    /// Whether nothing is remembered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Sixteen random bytes as lowercase hex, for a ticket nonce. `None` only when
/// the operating system cannot supply randomness, in which case no ticket
/// should be minted at all.
pub fn random_nonce_hex() -> Option<String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).ok()?;
    Some(hex::encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nonce_is_admitted_once() {
        let cache = NonceCache::new(8, 600);
        assert!(cache.admit("aa", 1_000, 1_030));
        assert!(!cache.admit("aa", 1_001, 1_030), "a second use is a replay");
        assert!(cache.admit("bb", 1_001, 1_030));
    }

    #[test]
    fn an_entry_outlives_the_ticket_that_carried_it() {
        // Retention is the floor; a ticket that can still verify later than
        // that keeps its record until it cannot.
        let cache = NonceCache::new(8, 600);
        assert!(cache.admit("aa", 1_000, 1_900));
        assert!(!cache.admit("aa", 1_650, 1_900));
        assert!(!cache.admit("aa", 1_900, 1_900));
        assert!(cache.admit("aa", 1_901, 2_000), "forgotten once unusable");
    }

    #[test]
    fn the_oldest_entry_is_evicted_at_capacity() {
        let cache = NonceCache::new(2, 600);
        assert!(cache.admit("a", 1_000, 1_030));
        assert!(cache.admit("b", 1_000, 1_030));
        assert!(cache.admit("c", 1_000, 1_030));
        assert_eq!(cache.len(), 2);
        assert!(cache.admit("a", 1_001, 1_030), "the oldest was evicted");
        assert!(!cache.admit("c", 1_001, 1_030));
    }

    #[test]
    fn expired_entries_are_purged_before_anything_live_is_evicted() {
        let cache = NonceCache::new(2, 10);
        assert!(cache.admit("old", 1_000, 1_000));
        assert!(cache.admit("live", 1_050, 1_100));
        // `old` is past its retention, so it goes and `live` stays.
        assert!(cache.admit("new", 1_060, 1_100));
        assert!(!cache.admit("live", 1_061, 1_100));
    }

    #[test]
    fn random_nonces_are_sixteen_bytes_of_hex_and_differ() {
        let a = random_nonce_hex().unwrap();
        let b = random_nonce_hex().unwrap();
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
