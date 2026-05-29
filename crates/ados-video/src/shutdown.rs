//! A minimal clone-able shutdown signal.
//!
//! The workspace deliberately carries no `tokio-util` (so no
//! `CancellationToken`); the supervisor expresses shutdown with a `select!`
//! over signal streams. This module gives the orchestrator the same ergonomics
//! with one `tokio::sync::Notify`: any number of clones can `wait()` on it, and
//! a single `trigger()` wakes them all and stays "fired" so a late waiter never
//! blocks forever.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::Notify;

/// A cancellation handle the run loop awaits and `main` triggers on SIGTERM /
/// SIGINT. Cheap to clone (shared `Arc`).
#[derive(Clone)]
pub struct Shutdown {
    fired: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl Shutdown {
    /// A fresh, un-fired shutdown handle.
    pub fn new() -> Self {
        Self {
            fired: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Fire the signal. Idempotent; wakes every current and future waiter.
    pub fn trigger(&self) {
        self.fired.store(true, Ordering::SeqCst);
        // notify_waiters wakes everyone currently parked; the fired flag
        // covers anyone who calls wait() after this point.
        self.notify.notify_waiters();
    }

    /// Has the signal fired?
    pub fn is_fired(&self) -> bool {
        self.fired.load(Ordering::SeqCst)
    }

    /// Resolve once the signal has fired. Returns immediately if it already
    /// fired before this call.
    pub async fn wait(&self) {
        if self.is_fired() {
            return;
        }
        // Register for the wakeup, then re-check to close the race where
        // trigger() ran between the flag check and the notified() await.
        let notified = self.notify.notified();
        if self.is_fired() {
            return;
        }
        notified.await;
    }
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn wait_resolves_after_trigger() {
        let s = Shutdown::new();
        let waiter = s.clone();
        let handle = tokio::spawn(async move { waiter.wait().await });
        // Give the task a moment to park on notified().
        tokio::time::sleep(Duration::from_millis(20)).await;
        s.trigger();
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("wait did not resolve after trigger")
            .expect("task panicked");
    }

    #[tokio::test]
    async fn wait_returns_immediately_if_already_fired() {
        let s = Shutdown::new();
        s.trigger();
        assert!(s.is_fired());
        // Must not block.
        tokio::time::timeout(Duration::from_millis(100), s.wait())
            .await
            .expect("wait blocked despite already-fired signal");
    }

    #[tokio::test]
    async fn multiple_clones_all_wake() {
        let s = Shutdown::new();
        let a = s.clone();
        let b = s.clone();
        let ha = tokio::spawn(async move { a.wait().await });
        let hb = tokio::spawn(async move { b.wait().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        s.trigger();
        let res = tokio::time::timeout(Duration::from_secs(1), async {
            ha.await.unwrap();
            hb.await.unwrap();
        })
        .await;
        assert!(res.is_ok(), "not all clones woke on trigger");
    }
}
