//! A latching, clone-able shutdown signal for a service's run loops.
//!
//! `tokio::sync::Notify::notify_waiters` stores no permit: it wakes only the
//! futures already waiting at the instant it runs. A loop that is inside its
//! body at that instant (building a snapshot, writing a frame) re-enters its
//! `select!` with a fresh `notified()` that nothing will ever wake, so a
//! shutdown that joins its tasks hangs until the service manager kills it.
//!
//! [`Shutdown`] latches instead: [`Shutdown::trigger`] sets a flag and wakes
//! every current waiter, and [`Shutdown::wait`] returns at once when the flag is
//! already set, so a loop that reaches its `select!` after the trigger still
//! stops.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::Notify;

/// A cancellation handle every run loop awaits and the service triggers on
/// SIGTERM / SIGINT. Cheap to clone (shared `Arc`s).
#[derive(Clone, Debug)]
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
        self.notify.notify_waiters();
    }

    /// Whether the signal has fired.
    pub fn is_fired(&self) -> bool {
        self.fired.load(Ordering::SeqCst)
    }

    /// Resolve once the signal has fired. Returns immediately when it fired
    /// before this call.
    pub async fn wait(&self) {
        if self.is_fired() {
            return;
        }
        // Register for the wakeup, then re-check, closing the race where
        // trigger() ran between the flag check and the registration.
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
    async fn a_parked_waiter_wakes_on_trigger() {
        let s = Shutdown::new();
        let waiter = s.clone();
        let handle = tokio::spawn(async move { waiter.wait().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        s.trigger();
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("a parked waiter must wake on trigger")
            .unwrap();
    }

    /// The property `Notify::notify_waiters` lacks: a loop that was busy when
    /// the signal fired, and only reaches its wait afterwards, still stops.
    #[tokio::test]
    async fn a_waiter_that_arrives_after_the_trigger_returns_at_once() {
        let s = Shutdown::new();
        s.trigger();
        tokio::time::timeout(Duration::from_millis(100), s.wait())
            .await
            .expect("a fired signal must not block a later waiter");
        assert!(s.is_fired());
    }
}
