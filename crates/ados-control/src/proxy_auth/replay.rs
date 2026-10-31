//! The nonce/timestamp replay detector.

use super::*;

/// The nonce/timestamp replay detector. Holds a `nonce -> timestamp` map behind
/// a mutex (the front shares one detector across all connections), pruning
/// expired nonces every `window` seconds, or whenever the store exceeds
/// `max_nonces`.
pub struct ReplayDetector {
    pub(super) window: f64,
    pub(super) max_nonces: usize,
    pub(super) state: Mutex<ReplayState>,
}

pub(super) struct ReplayState {
    pub(super) nonces: HashMap<String, f64>,
    pub(super) last_prune: f64,
}

impl ReplayDetector {
    /// A detector with the given window (seconds) and nonce cap.
    pub fn new(window_seconds: f64, max_nonces: usize) -> Self {
        Self {
            window: window_seconds,
            max_nonces,
            state: Mutex::new(ReplayState {
                nonces: HashMap::new(),
                last_prune: 0.0,
            }),
        }
    }

    /// The 300 s / 50000-nonce detector the proxied-route HMAC gate uses.
    pub fn default_security() -> Self {
        Self::new(REPLAY_WINDOW_SECONDS, REPLAY_MAX_NONCES)
    }

    /// True when the message is fresh (timestamp within the window AND the nonce
    /// is unseen), false when it should be rejected. A fresh message records its
    /// nonce and prunes when due. `now` is supplied so a test can pin the clock.
    pub fn check_at(&self, timestamp: f64, nonce: &str, now: f64) -> bool {
        // Reject messages outside the time window.
        let age = (now - timestamp).abs();
        if age > self.window {
            return false;
        }
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        // Reject duplicate nonces.
        if s.nonces.contains_key(nonce) {
            return false;
        }
        // Store the nonce.
        s.nonces.insert(nonce.to_string(), timestamp);
        // Prune if due (every window, or when the store is oversized).
        if now - s.last_prune > self.window || s.nonces.len() > self.max_nonces {
            let cutoff = now - self.window;
            s.nonces.retain(|_, ts| *ts >= cutoff);
            s.last_prune = now;
        }
        true
    }

    /// `check_at` against the real clock.
    pub fn check(&self, timestamp: f64, nonce: &str) -> bool {
        self.check_at(timestamp, nonce, unix_now())
    }
}

/// Wall-clock seconds since the epoch as `f64`.
pub(super) fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}
