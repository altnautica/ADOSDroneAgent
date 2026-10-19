//! Front-panel button events, read off the `ados-pic` fanout socket.
//!
//! The panel's four tactile buttons are read, debounced and decoded by
//! `ados-hid` (`PressClassifier`), which publishes each release-edge event onto
//! `/run/ados/buttons.sock` as newline-delimited JSON. [`PageNavigator::on_button`]
//! has always known what to do with those events — this module is the transport
//! that carries them from the socket to the render loop, which is what makes a
//! physical press change the page.
//!
//! [`PageNavigator::on_button`]: crate::navigator::PageNavigator::on_button
//!
//! Two deliberate properties:
//!
//! * **Parsing is pure.** [`parse_button_line`] maps one wire line to a
//!   [`ButtonEvent`] with no I/O, so the wire contract is unit-tested against
//!   the exact bytes `ados-hid` emits rather than against a mock.
//! * **An absent socket is not an error.** A ground station whose `ados-pic` is
//!   not running (no gpiochip, or the service is down) must still render. The
//!   reader retries at the fixed local-socket pace and the UI never notices.
//!
//! Decoding deliberately does NOT happen here. Short/long/cancel and the
//! action mapping are owned by `ados-hid`. The one exception is a press that
//! arrives with no action because its pin is a board-declared button outside the
//! fixed pin set: [`ButtonLayout`] resolves it through the same mapping slots.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub use ados_hid::buttons::board_button_pins;
use ados_hid::buttons::{
    board_pins_or_default, default_button_mapping, merge_mapping, ButtonEvent, PressKind,
    BUTTON_PINS,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

/// Default path of the `ados-pic` button fanout socket.
pub const BUTTONS_SOCK: &str = "/run/ados/buttons.sock";

/// Depth of the channel from the reader task to the render loop. Presses are a
/// human-rate event; anything beyond a handful queued means the UI is wedged, and
/// dropping the excess is better than growing without bound.
const CHANNEL_DEPTH: usize = 16;

/// The board's four front-panel buttons and the action map they resolve
/// through.
///
/// The pins come from the HAL board sidecar's `buttons` block (`{"up","down",
/// "select","back"}`, the board's GPIO numbers) through the same resolution the
/// `ados-pic` reader uses, else the fixed [`BUTTON_PINS`]. They are matched in
/// that order to the `B1`..`B4`
/// mapping slots. An event that arrives without an action (its pin is not one
/// of the fixed lines, so the publisher had no slot for it) is resolved here
/// through the configured mapping; an event that already carries an action is
/// passed through untouched.
#[derive(Debug, Clone, PartialEq)]
pub struct ButtonLayout {
    pub pins: [u32; 4],
    mapping: HashMap<String, String>,
}

impl Default for ButtonLayout {
    fn default() -> Self {
        Self {
            pins: BUTTON_PINS,
            mapping: default_button_mapping("ground_station"),
        }
    }
}

impl ButtonLayout {
    /// Read the pins from `board_sidecar` and the mapping from the agent config
    /// (`$ADOS_CONFIG`, default `/etc/ados/config.yaml`) over the defaults of the
    /// node's resolved profile. Any missing or malformed source falls back to
    /// the built-in default for that part.
    pub fn load(board_sidecar: &Path) -> Self {
        let pins = board_pins_or_default(board_sidecar);
        let profile = ados_config::node_profile().replace('-', "_");
        let config =
            std::env::var("ADOS_CONFIG").unwrap_or_else(|_| "/etc/ados/config.yaml".to_string());
        let mapping = std::fs::read_to_string(config)
            .ok()
            .and_then(|text| serde_norway::from_str::<serde_norway::Value>(&text).ok())
            .and_then(|root| {
                root.get("ground_station")?
                    .get("ui")?
                    .get("buttons")?
                    .get("mapping")
                    .cloned()
            })
            .map(|block| merge_mapping(&profile, &block))
            .unwrap_or_else(|| default_button_mapping(&profile));
        Self { pins, mapping }
    }

    /// Fill in the action of an event that arrived unmapped, when its pin is one
    /// of this board's buttons.
    pub fn resolve(&self, mut event: ButtonEvent) -> ButtonEvent {
        if event.action.is_some() {
            return event;
        }
        if let Some(slot) = self.pins.iter().position(|&p| p == event.pin) {
            let kind = match event.kind {
                PressKind::Short => "short",
                PressKind::Long => "long",
            };
            let key = format!("B{}_{kind}", slot + 1);
            event.action = self.mapping.get(&key).cloned();
        }
        event
    }

    /// The mapping key (`B1_long`, `B2_short`, ...) bound to `action`, so a
    /// page can name the button that answers it. When several keys carry the
    /// action the lowest button wins, and a short press before a long one.
    pub fn key_for(&self, action: &str) -> Option<&str> {
        self.mapping
            .iter()
            .filter(|(_, a)| a.as_str() == action)
            .map(|(k, _)| k.as_str())
            .min_by_key(|k| (button_number(k), k.ends_with("_long")))
    }
}

/// The button number of a `B<n>_<kind>` key; unparsable keys sort last.
fn button_number(key: &str) -> u32 {
    key.strip_prefix('B')
        .and_then(|rest| rest.split('_').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(u32::MAX)
}

/// Parse one line of the `buttons.sock` stream into a [`ButtonEvent`].
///
/// The wire shape, emitted by `ados_hid::buttons_ipc`:
///
/// ```json
/// {"button":13,"kind":"long","action":"pair_drone","timestamp_ms":4242}
/// ```
///
/// `action` may be JSON `null` for an unmapped button, which round-trips to
/// `None` — the navigator treats that as a no-op rather than guessing. Returns
/// `None` for a blank line, malformed JSON, a missing/!u32 `button`, or a `kind`
/// that is not exactly `short` or `long`; the caller skips such a line rather
/// than tearing down the connection, because one bad frame is not a dead socket.
pub fn parse_button_line(line: &str) -> Option<ButtonEvent> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;

    let pin = u32::try_from(v.get("button")?.as_u64()?).ok()?;
    let kind = match v.get("kind")?.as_str()? {
        "short" => PressKind::Short,
        "long" => PressKind::Long,
        _ => return None,
    };
    // Absent and explicit-null both mean "unmapped"; a non-string action is
    // malformed and drops the line.
    let action = match v.get("action") {
        None | Some(serde_json::Value::Null) => None,
        Some(other) => Some(other.as_str()?.to_string()),
    };
    let timestamp_ms = v.get("timestamp_ms").and_then(|t| t.as_u64()).unwrap_or(0);

    Some(ButtonEvent {
        pin,
        kind,
        action,
        timestamp_ms,
    })
}

/// Connect to `sock_path`, subscribe, and forward each decoded press, resolved
/// through `layout`.
///
/// Returns immediately with the receiving half; the reader runs as a task and
/// reconnects at the fixed local-socket pace for the lifetime of the process. The channel
/// closes only if the caller drops the receiver.
pub fn spawn_button_reader(
    sock_path: PathBuf,
    layout: ButtonLayout,
) -> mpsc::Receiver<ButtonEvent> {
    let (tx, rx) = mpsc::channel(CHANNEL_DEPTH);
    tokio::spawn(async move {
        loop {
            match read_until_disconnect(&sock_path, &layout, &tx).await {
                Ok(()) => tracing::debug!("button stream closed; reconnecting"),
                Err(e) => tracing::debug!(error = %e, "button stream unavailable"),
            }
            if tx.is_closed() {
                return;
            }
            tokio::time::sleep(ados_protocol::retry::LOCAL_SOCKET.wait()).await;
        }
    });
    rx
}

/// One connection's lifetime: subscribe, then pump lines until EOF or error.
async fn read_until_disconnect(
    sock_path: &PathBuf,
    layout: &ButtonLayout,
    tx: &mpsc::Sender<ButtonEvent>,
) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(sock_path).await?;
    stream.write_all(b"{\"op\":\"subscribe\"}\n").await?;
    stream.flush().await?;

    let mut lines = BufReader::new(stream).lines();
    while let Some(line) = lines.next_line().await? {
        let Some(event) = parse_button_line(&line) else {
            // A malformed frame is skipped, not fatal: the stream is shared and
            // a future field addition must not take the panel's input away.
            continue;
        };
        if tx.send(layout.resolve(event)).await.is_err() {
            // The render loop is gone; stop cleanly.
            return Ok(());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_exact_wire_shape_ados_hid_emits() {
        let ev = parse_button_line(
            r#"{"button":13,"kind":"long","action":"pair_drone","timestamp_ms":4242}"#,
        )
        .expect("well-formed line parses");
        assert_eq!(ev.pin, 13);
        assert_eq!(ev.kind, PressKind::Long);
        assert_eq!(ev.action.as_deref(), Some("pair_drone"));
        assert_eq!(ev.timestamp_ms, 4242);
    }

    #[test]
    fn a_null_action_is_an_unmapped_button_not_a_parse_failure() {
        // ados-hid emits an explicit null for a button with no mapping; the
        // navigator must see the press and decide it is inert.
        let ev = parse_button_line(r#"{"button":5,"kind":"short","action":null,"timestamp_ms":1}"#)
            .expect("null action still yields an event");
        assert_eq!(ev.action, None);
    }

    #[test]
    fn short_and_long_are_the_only_accepted_kinds() {
        // Both arms are asserted deliberately: an earlier version of this test
        // checked only `short`, and a mutation that decoded `long` as `Short`
        // passed it. A long press is the gesture that reaches the guarded
        // actions, so conflating the two is exactly the bug worth catching.
        assert_eq!(
            parse_button_line(r#"{"button":5,"kind":"short","timestamp_ms":1}"#).map(|e| e.kind),
            Some(PressKind::Short)
        );
        assert_eq!(
            parse_button_line(r#"{"button":5,"kind":"long","timestamp_ms":1}"#).map(|e| e.kind),
            Some(PressKind::Long)
        );
        // A future third kind must not silently decode as one of the two we know.
        assert!(parse_button_line(r#"{"button":5,"kind":"double","timestamp_ms":1}"#).is_none());
    }

    #[test]
    fn malformed_lines_are_skipped_rather_than_guessed() {
        assert!(parse_button_line("").is_none());
        assert!(parse_button_line("   ").is_none());
        assert!(parse_button_line("not json").is_none());
        assert!(parse_button_line("{}").is_none());
        // Missing kind.
        assert!(parse_button_line(r#"{"button":5}"#).is_none());
        // Missing button.
        assert!(parse_button_line(r#"{"kind":"short"}"#).is_none());
        // Negative pin cannot be a BCM line.
        assert!(parse_button_line(r#"{"button":-1,"kind":"short"}"#).is_none());
        // A non-string action is malformed, not "unmapped".
        assert!(parse_button_line(r#"{"button":5,"kind":"short","action":7}"#).is_none());
    }

    #[test]
    fn a_missing_timestamp_defaults_rather_than_dropping_the_press() {
        let ev = parse_button_line(r#"{"button":6,"kind":"short","action":"back"}"#)
            .expect("timestamp is not load-bearing for dispatch");
        assert_eq!(ev.timestamp_ms, 0);
        assert_eq!(ev.action.as_deref(), Some("back"));
    }

    #[test]
    fn board_buttons_block_overrides_the_fixed_pins() {
        assert_eq!(
            board_button_pins(
                r#"{"name":"example-oem board","buttons":{"up":17,"down":27,"select":22,"back":23}}"#
            ),
            Some([17, 27, 22, 23])
        );
        assert_eq!(
            board_button_pins(r#"{"board":{"buttons":{"up":1,"down":2,"select":3,"back":4}}}"#),
            Some([1, 2, 3, 4])
        );
        // No block, or an incomplete one, keeps the fixed pins.
        assert_eq!(board_button_pins(r#"{"name":"x"}"#), None);
        assert_eq!(board_button_pins(r#"{"buttons":{"up":17}}"#), None);
        assert_eq!(ButtonLayout::default().pins, BUTTON_PINS);
    }

    #[test]
    fn unmapped_press_on_a_board_pin_resolves_through_its_slot() {
        let layout = ButtonLayout {
            pins: [17, 27, 22, 23],
            mapping: default_button_mapping("ground_station"),
        };
        let back = parse_button_line(r#"{"button":23,"kind":"short","action":null}"#).unwrap();
        assert_eq!(layout.resolve(back).action.as_deref(), Some("back"));
        let pair = parse_button_line(r#"{"button":22,"kind":"long"}"#).unwrap();
        assert_eq!(layout.resolve(pair).action.as_deref(), Some("pair_drone"));
        // A pin outside the board's set stays unmapped; a mapped press is kept.
        let stray = parse_button_line(r#"{"button":4,"kind":"short"}"#).unwrap();
        assert_eq!(layout.resolve(stray).action, None);
        let mapped = parse_button_line(r#"{"button":23,"kind":"short","action":"menu"}"#).unwrap();
        assert_eq!(layout.resolve(mapped).action.as_deref(), Some("menu"));
    }

    #[test]
    fn key_for_names_the_button_bound_to_an_action() {
        let visor = ButtonLayout {
            pins: BUTTON_PINS,
            mapping: default_button_mapping("visor"),
        };
        assert_eq!(visor.key_for("shutdown_prompt"), Some("B1_long"));
        assert_eq!(visor.key_for("back"), Some("B4_short"));
        // The ground-station default has no shutdown binding.
        assert_eq!(ButtonLayout::default().key_for("shutdown_prompt"), None);
        let loaded: serde_norway::Value =
            serde_norway::from_str("B3_short: shutdown_prompt\nB2_long: shutdown_prompt\n")
                .unwrap();
        let remapped = ButtonLayout {
            pins: BUTTON_PINS,
            mapping: merge_mapping("ground_station", &loaded),
        };
        assert_eq!(remapped.key_for("shutdown_prompt"), Some("B2_long"));
    }
}
