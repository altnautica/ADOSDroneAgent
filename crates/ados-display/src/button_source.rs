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
//!   reader retries on a bounded backoff and the UI never notices.
//!
//! Decoding deliberately does NOT happen here. Short/long/cancel and the
//! action mapping are owned by `ados-hid`; duplicating any of that would give the
//! panel a second, drifting opinion about what a press means.

use std::path::PathBuf;
use std::time::Duration;

use ados_hid::buttons::{ButtonEvent, PressKind};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

/// Default path of the `ados-pic` button fanout socket.
pub const BUTTONS_SOCK: &str = "/run/ados/buttons.sock";

/// Depth of the channel from the reader task to the render loop. Presses are a
/// human-rate event; anything beyond a handful queued means the UI is wedged, and
/// dropping the excess is better than growing without bound.
const CHANNEL_DEPTH: usize = 16;

const RECONNECT_MIN: Duration = Duration::from_millis(500);
const RECONNECT_MAX: Duration = Duration::from_secs(10);

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

/// Connect to `sock_path`, subscribe, and forward each decoded press.
///
/// Returns immediately with the receiving half; the reader runs as a task and
/// reconnects on a bounded backoff for the lifetime of the process. The channel
/// closes only if the caller drops the receiver.
pub fn spawn_button_reader(sock_path: PathBuf) -> mpsc::Receiver<ButtonEvent> {
    let (tx, rx) = mpsc::channel(CHANNEL_DEPTH);
    tokio::spawn(async move {
        let mut backoff = RECONNECT_MIN;
        loop {
            match read_until_disconnect(&sock_path, &tx).await {
                Ok(()) => {
                    // Clean EOF: ados-pic restarted. Reset the backoff so a
                    // service restart is picked up promptly.
                    backoff = RECONNECT_MIN;
                    tracing::debug!("button stream closed; reconnecting");
                }
                Err(e) => {
                    tracing::debug!(error = %e, "button stream unavailable");
                }
            }
            if tx.is_closed() {
                return;
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(RECONNECT_MAX);
        }
    });
    rx
}

/// One connection's lifetime: subscribe, then pump lines until EOF or error.
async fn read_until_disconnect(
    sock_path: &PathBuf,
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
        if tx.send(event).await.is_err() {
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
}
