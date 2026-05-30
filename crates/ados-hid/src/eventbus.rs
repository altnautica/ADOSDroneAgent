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

/// Per-subscriber depth for the button bus. Matches the Python button bus
/// `queue_maxsize` default (64).
pub const BUTTON_EVENT_QUEUE_DEPTH: usize = 64;

/// A classified front-panel button press, in the shape the display/OLED
/// consumer reads off the wire. Field names + types match the Python
/// `ButtonEvent` (`button`, `kind`, `timestamp_ms`, `action`) so a Python
/// consumer parses it with `json.loads` and a Rust consumer with serde.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ButtonBusEvent {
    /// BCM pin number.
    pub button: u32,
    /// "short" or "long".
    pub kind: &'static str,
    /// Resolved action name from the live mapping; `None` when unmapped.
    pub action: Option<String>,
    /// Release-edge timestamp, monotonic milliseconds.
    pub timestamp_ms: u64,
}

/// Fanout bus for [`ButtonBusEvent`]. The `ados-pic` daemon's button reader is
/// the publisher; the display/OLED layer subscribes over the control socket
/// (`subscribe_buttons`). Same drop-on-lag broadcast contract as
/// [`PicEventBus`].
#[derive(Debug, Clone)]
pub struct ButtonEventBus {
    tx: broadcast::Sender<ButtonBusEvent>,
}

impl ButtonEventBus {
    /// Build a bus with the default per-subscriber depth.
    pub fn new() -> Self {
        Self::with_depth(BUTTON_EVENT_QUEUE_DEPTH)
    }

    /// Build a bus with an explicit per-subscriber depth (used in tests).
    pub fn with_depth(depth: usize) -> Self {
        let (tx, _rx) = broadcast::channel(depth.max(1));
        Self { tx }
    }

    /// A fresh receiver. Each subscriber drains independently; a subscriber
    /// that lags more than the depth sees `RecvError::Lagged`.
    pub fn subscribe(&self) -> broadcast::Receiver<ButtonBusEvent> {
        self.tx.subscribe()
    }

    /// Publish one event. Returns the number of live receivers it reached; a
    /// send with no receivers returns 0 (drop-on-empty).
    pub fn publish(&self, event: ButtonBusEvent) -> usize {
        self.tx.send(event).unwrap_or(0)
    }

    /// Current live receiver count.
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for ButtonEventBus {
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

    #[tokio::test]
    async fn button_bus_fans_out_to_each_subscriber() {
        let bus = ButtonEventBus::new();
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();
        let event = ButtonBusEvent {
            button: 5,
            kind: "short",
            action: Some("cycle_screen".into()),
            timestamp_ms: 1500,
        };
        assert_eq!(bus.publish(event.clone()), 2);
        assert_eq!(a.recv().await.unwrap(), event);
        assert_eq!(b.recv().await.unwrap(), event);
    }

    #[test]
    fn button_bus_publish_with_no_subscribers_is_a_noop() {
        let bus = ButtonEventBus::new();
        let event = ButtonBusEvent {
            button: 6,
            kind: "long",
            action: None,
            timestamp_ms: 2000,
        };
        assert_eq!(bus.publish(event), 0);
    }

    #[tokio::test]
    async fn button_bus_slow_subscriber_is_lagged_not_blocking() {
        let bus = ButtonEventBus::with_depth(2);
        let mut slow = bus.subscribe();
        for i in 0..5u32 {
            bus.publish(ButtonBusEvent {
                button: i,
                kind: "short",
                action: None,
                timestamp_ms: i as u64,
            });
        }
        match slow.recv().await {
            Err(broadcast::error::RecvError::Lagged(n)) => assert!(n >= 1),
            other => panic!("expected Lagged, got {other:?}"),
        }
    }
}
