//! In-process fanout for PIC state transitions.
//!
//! Mirrors the Button / PIC / Input event buses on the Python side: every
//! subscriber gets its own bounded receiver, and a slow consumer is lagged
//! (drops the oldest events) rather than allowed to stall the publisher. Built
//! on `tokio::sync::broadcast`, matching the `ados-plugin-host` `EventBus`
//! shape.

use tokio::sync::broadcast;

/// Per-subscriber bus depth. Matches the Python `PicEventBus` `queue_maxsize`
/// default (64): a subscriber that falls more than this many events behind is
/// lagged rather than blocking the publisher.
pub const PIC_EVENT_QUEUE_DEPTH: usize = 64;

/// The kind of PIC transition an event carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PicEventKind {
    /// A client became the PIC holder (fresh, forced, or confirmed takeover).
    Claimed,
    /// The PIC holder released control voluntarily or via the watchdog.
    Released,
    /// The PIC holder dropped (WS close or gamepad removal).
    Disconnected,
}

impl PicEventKind {
    /// The wire string used on the IPC/REST surface.
    pub fn as_str(self) -> &'static str {
        match self {
            PicEventKind::Claimed => "claimed",
            PicEventKind::Released => "released",
            PicEventKind::Disconnected => "disconnected",
        }
    }
}

/// A single PIC state-transition observation.
///
/// `client_id` is the new holder on `Claimed`, and the client that just lost
/// PIC on `Released` / `Disconnected`. `claim_counter` is the monotonic counter
/// so clients can detect races when they observe the bus out of order relative
/// to REST replies. `timestamp_ms` is unix milliseconds at the transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PicEvent {
    pub kind: PicEventKind,
    pub client_id: Option<String>,
    pub claim_counter: u64,
    pub timestamp_ms: i64,
}

/// Fanout bus for [`PicEvent`].
#[derive(Debug, Clone)]
pub struct PicEventBus {
    tx: broadcast::Sender<PicEvent>,
}

impl PicEventBus {
    /// Build a bus with the default per-subscriber depth.
    pub fn new() -> Self {
        Self::with_depth(PIC_EVENT_QUEUE_DEPTH)
    }

    /// Build a bus with an explicit per-subscriber depth (used in tests).
    pub fn with_depth(depth: usize) -> Self {
        let (tx, _rx) = broadcast::channel(depth.max(1));
        Self { tx }
    }

    /// A fresh receiver. Each subscriber drains independently; a subscriber
    /// that lags more than the depth sees `RecvError::Lagged` rather than
    /// stalling the publisher.
    pub fn subscribe(&self) -> broadcast::Receiver<PicEvent> {
        self.tx.subscribe()
    }

    /// Publish an event. Returns the number of live receivers it reached. A
    /// send with no receivers returns 0 rather than erroring (drop-on-empty),
    /// matching the Python `publish` no-op when there are no subscribers.
    pub fn publish(&self, event: PicEvent) -> usize {
        self.tx.send(event).unwrap_or(0)
    }

    /// Current live receiver count.
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for PicEventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn publish_reaches_each_subscriber() {
        let bus = PicEventBus::new();
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();
        let event = PicEvent {
            kind: PicEventKind::Claimed,
            client_id: Some("op-1".into()),
            claim_counter: 1,
            timestamp_ms: 42,
        };
        assert_eq!(bus.publish(event.clone()), 2);
        assert_eq!(a.recv().await.unwrap(), event);
        assert_eq!(b.recv().await.unwrap(), event);
    }

    #[test]
    fn publish_with_no_subscribers_is_a_noop() {
        let bus = PicEventBus::new();
        let event = PicEvent {
            kind: PicEventKind::Released,
            client_id: None,
            claim_counter: 0,
            timestamp_ms: 0,
        };
        assert_eq!(bus.publish(event), 0);
    }

    #[tokio::test]
    async fn slow_subscriber_is_lagged_not_blocking() {
        // Depth 2: the publisher never blocks; a subscriber that does not keep
        // up sees a Lagged error instead of stalling everyone else.
        let bus = PicEventBus::with_depth(2);
        let mut slow = bus.subscribe();
        for i in 0..5 {
            bus.publish(PicEvent {
                kind: PicEventKind::Claimed,
                client_id: Some(format!("op-{i}")),
                claim_counter: i,
                timestamp_ms: i as i64,
            });
        }
        // First drain reports the lag rather than blocking the publisher.
        match slow.recv().await {
            Err(broadcast::error::RecvError::Lagged(n)) => assert!(n >= 1),
            other => panic!("expected Lagged, got {other:?}"),
        }
    }

    #[test]
    fn kind_wire_strings_match_python() {
        assert_eq!(PicEventKind::Claimed.as_str(), "claimed");
        assert_eq!(PicEventKind::Released.as_str(), "released");
        assert_eq!(PicEventKind::Disconnected.as_str(), "disconnected");
    }
}
