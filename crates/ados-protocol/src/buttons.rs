//! Front-panel button events as they reach a plugin.
//!
//! The physical read, debounce and short/long decode all belong to `ados-hid`,
//! which publishes each release-edge event on the button fanout socket. This
//! module is only the plugin-facing half: the two method names and the shape of
//! the event the host pushes.
//!
//! It lives here rather than in the host or either SDK because three separate
//! implementations have to agree on it — the Rust host that pushes, and the
//! Python and Rust SDKs that receive — and a wire contract with three
//! independent spellings is a contract that drifts.
//!
//! # Why a push and not a poll
//!
//! A button press is an edge, not a level: a plugin that polls either misses
//! presses between polls or has to be told about them anyway. So the host arms a
//! per-connection stream on [`SUBSCRIBE`] and pushes a [`DELIVER`] event per
//! press, the same shape the vision subscriptions use.
//!
//! # What a subscriber does and does not get
//!
//! Read-only, and deliberately not exclusive. Several consumers watch the same
//! bus — the display navigator and the websocket relay among them — so a
//! subscribing plugin observes presses and neither consumes them nor remaps
//! them. A plugin that wants a button to stop doing its normal thing has to be
//! given that by configuration, not by subscribing.

use serde::{Deserialize, Serialize};

/// Wire name of the subscribe request.
pub const SUBSCRIBE: &str = "button.subscribe";

/// Wire name of the pushed event. Carries a [`ButtonPress`] under `press`.
pub const DELIVER: &str = "button.deliver";

/// One decoded front-panel press.
///
/// `pin` is the raw BCM line rather than a friendly label so the identity stays
/// stable if the label table is ever re-spelled; `action` is the host's live
/// mapping for that (pin, kind) pair and is `None` for an unmapped button — a
/// subscriber sees the press either way and decides for itself whether an
/// unmapped button means anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ButtonPress {
    /// BCM pin number of the button.
    pub pin: u32,
    /// `"short"` or `"long"`, decoded by the host on the release edge.
    pub kind: String,
    /// Resolved action name from the live mapping; `None` when unmapped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// Release-edge timestamp, monotonic milliseconds since the reader started.
    #[serde(default)]
    pub timestamp_ms: u64,
}

impl ButtonPress {
    /// True when this is a long press.
    ///
    /// A helper rather than an enum on the wire: the host's own bus carries the
    /// kind as a string, and re-typing it here would mean a third spelling to
    /// keep in step for no gain.
    pub fn is_long(&self) -> bool {
        self.kind == "long"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unmapped_button_round_trips_without_an_action_field() {
        let press = ButtonPress {
            pin: 5,
            kind: "short".to_string(),
            action: None,
            timestamp_ms: 12,
        };
        let json = serde_json::to_string(&press).unwrap();
        assert!(
            !json.contains("action"),
            "an absent action is omitted, not sent as null: {json}"
        );
        assert_eq!(serde_json::from_str::<ButtonPress>(&json).unwrap(), press);
    }

    #[test]
    fn a_mapped_button_round_trips_with_its_action() {
        let press = ButtonPress {
            pin: 13,
            kind: "long".to_string(),
            action: Some("pair_drone".to_string()),
            timestamp_ms: 4242,
        };
        let json = serde_json::to_string(&press).unwrap();
        assert_eq!(serde_json::from_str::<ButtonPress>(&json).unwrap(), press);
        assert!(press.is_long());
    }

    #[test]
    fn an_explicit_null_action_decodes_as_unmapped() {
        // The host's fanout socket emits `"action": null` for an unmapped
        // button, so the receiving side must accept that spelling too and not
        // just the omitted form.
        let press: ButtonPress =
            serde_json::from_str(r#"{"pin":6,"kind":"short","action":null,"timestamp_ms":1}"#)
                .unwrap();
        assert_eq!(press.action, None);
        assert!(!press.is_long());
    }

    #[test]
    fn a_missing_timestamp_defaults_rather_than_failing_the_decode() {
        let press: ButtonPress = serde_json::from_str(r#"{"pin":19,"kind":"short"}"#).unwrap();
        assert_eq!(press.timestamp_ms, 0);
    }
}
