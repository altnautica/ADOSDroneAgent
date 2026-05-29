//! Uplink event types and fanout bus.
//!
//! [`UplinkEvent`] is the canonical record for routing, health, and data-cap
//! state changes. [`UplinkEventBus`] mirrors the Python `UplinkEventBus`:
//! bounded fanout, drop-on-lag. The Python side keeps bounded per-subscriber
//! queues with drop-on-full; `tokio::sync::broadcast` is the idiomatic
//! equivalent — a bounded ring per channel where a slow receiver loses the
//! oldest events (a `Lagged` skip) rather than blocking the publisher.

use serde::Serialize;
use tokio::sync::broadcast;

/// Routing / health / data-cap event kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UplinkEventKind {
    UplinkChanged,
    HealthChanged,
    DataCapThreshold,
}

/// Data-cap state ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DataCapState {
    Ok,
    #[serde(rename = "warn_80")]
    Warn80,
    #[serde(rename = "throttle_95")]
    Throttle95,
    #[serde(rename = "blocked_100")]
    Blocked100,
}

/// A routing, health, or data-cap state change. Field set matches the Python
/// `UplinkEvent` dataclass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UplinkEvent {
    pub kind: UplinkEventKind,
    pub active_uplink: Option<String>,
    pub available: Vec<String>,
    pub internet_reachable: bool,
    pub data_cap_state: Option<DataCapState>,
    pub timestamp_ms: u64,
}

/// Default per-channel ring capacity (matches the Python `queue_maxsize=64`).
pub const DEFAULT_QUEUE_MAXSIZE: usize = 64;

/// Fanout bus for [`UplinkEvent`]. Bounded ring, drop-oldest on lag.
#[derive(Debug)]
pub struct UplinkEventBus {
    tx: broadcast::Sender<UplinkEvent>,
}

impl UplinkEventBus {
    /// Build a bus with the default ring capacity.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_QUEUE_MAXSIZE)
    }

    /// Build a bus with an explicit ring capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity.max(1));
        Self { tx }
    }

    /// Publish an event. Returns the number of live subscribers it reached
    /// (zero is fine — drop-on-no-subscribers, like the Python publish).
    pub fn publish(&self, event: UplinkEvent) -> usize {
        self.tx.send(event).unwrap_or(0)
    }

    /// Subscribe a new receiver. A lagging receiver sees `RecvError::Lagged`
    /// and may continue, mirroring the Python drop-on-full semantics.
    pub fn subscribe(&self) -> broadcast::Receiver<UplinkEvent> {
        self.tx.subscribe()
    }
}

impl Default for UplinkEventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evt(kind: UplinkEventKind, active: Option<&str>) -> UplinkEvent {
        UplinkEvent {
            kind,
            active_uplink: active.map(|s| s.to_string()),
            available: vec!["eth0".to_string()],
            internet_reachable: true,
            data_cap_state: None,
            timestamp_ms: 1234,
        }
    }

    #[test]
    fn kind_and_cap_state_serialize_to_python_strings() {
        assert_eq!(
            serde_json::to_value(UplinkEventKind::UplinkChanged).unwrap(),
            "uplink_changed"
        );
        assert_eq!(
            serde_json::to_value(UplinkEventKind::HealthChanged).unwrap(),
            "health_changed"
        );
        assert_eq!(
            serde_json::to_value(UplinkEventKind::DataCapThreshold).unwrap(),
            "data_cap_threshold"
        );
        assert_eq!(serde_json::to_value(DataCapState::Ok).unwrap(), "ok");
        assert_eq!(
            serde_json::to_value(DataCapState::Warn80).unwrap(),
            "warn_80"
        );
        assert_eq!(
            serde_json::to_value(DataCapState::Throttle95).unwrap(),
            "throttle_95"
        );
        assert_eq!(
            serde_json::to_value(DataCapState::Blocked100).unwrap(),
            "blocked_100"
        );
    }

    #[tokio::test]
    async fn publish_fans_out_to_subscribers() {
        let bus = UplinkEventBus::new();
        let mut rx = bus.subscribe();
        let reached = bus.publish(evt(UplinkEventKind::UplinkChanged, Some("eth0")));
        assert_eq!(reached, 1);
        let got = rx.recv().await.unwrap();
        assert_eq!(got.kind, UplinkEventKind::UplinkChanged);
        assert_eq!(got.active_uplink.as_deref(), Some("eth0"));
    }

    #[test]
    fn publish_with_no_subscribers_is_a_noop() {
        let bus = UplinkEventBus::new();
        assert_eq!(bus.publish(evt(UplinkEventKind::HealthChanged, None)), 0);
    }
}
