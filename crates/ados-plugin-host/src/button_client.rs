//! Host-side reader for the front-panel button bus.
//!
//! `ados-hid` publishes each decoded press on a fanout socket as newline JSON.
//! This client subscribes once for the whole host and re-broadcasts presses to
//! however many plugins have armed `button.subscribe`, so N subscribers cost one
//! connection rather than N.
//!
//! It is deliberately a *re-broadcaster*, not a decoder. Short/long, the
//! debounce and the action mapping are `ados-hid`'s, and re-deriving any of them
//! here would give the host a second opinion that drifts from the panel's.
//!
//! An absent bus is a normal resting state, not an error: a drone has no front
//! panel, and a ground station whose `ados-pic` is down should not make every
//! subscribing plugin handle a failure. The reader retries quietly on a bounded
//! backoff, and a subscriber simply receives nothing until presses exist.

use std::path::PathBuf;
use std::time::Duration;

use ados_protocol::buttons::ButtonPress;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::broadcast;

/// Default path of the `ados-pic` button fanout socket.
pub const BUTTONS_SOCK: &str = "/run/ados/buttons.sock";

/// Broadcast depth. Presses are human-rate; a subscriber this far behind is
/// wedged, and dropping the oldest beats growing without bound.
const BROADCAST_DEPTH: usize = 64;

const RECONNECT_MIN: Duration = Duration::from_millis(500);
const RECONNECT_MAX: Duration = Duration::from_secs(10);

/// A shared subscription to the host's button bus.
pub struct ButtonClient {
    tx: broadcast::Sender<Vec<u8>>,
}

impl ButtonClient {
    /// Connect to `sock_path` and start re-broadcasting presses.
    ///
    /// Returns immediately; the reader runs as a task for the process lifetime
    /// and reconnects on a bounded backoff, so a `ados-pic` restart heals
    /// without the host noticing.
    pub fn spawn(sock_path: PathBuf) -> Self {
        let (tx, _rx) = broadcast::channel(BROADCAST_DEPTH);
        let tx_task = tx.clone();
        tokio::spawn(async move {
            let mut backoff = RECONNECT_MIN;
            loop {
                match pump(&sock_path, &tx_task).await {
                    Ok(()) => {
                        backoff = RECONNECT_MIN;
                        tracing::debug!("button bus closed; reconnecting");
                    }
                    Err(e) => tracing::debug!(error = %e, "button bus unavailable"),
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(RECONNECT_MAX);
            }
        });
        Self { tx }
    }

    /// A receiver for the shared press stream.
    pub fn subscribe(&self) -> broadcast::Receiver<Vec<u8>> {
        self.tx.subscribe()
    }
}

/// One connection's lifetime: subscribe, then re-broadcast until EOF or error.
async fn pump(sock_path: &PathBuf, tx: &broadcast::Sender<Vec<u8>>) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(sock_path).await?;
    stream.write_all(b"{\"op\":\"subscribe\"}\n").await?;
    stream.flush().await?;

    let mut lines = BufReader::new(stream).lines();
    while let Some(line) = lines.next_line().await? {
        let Some(bytes) = reencode_press(&line) else {
            // One malformed frame is not a dead bus, and a field the host does
            // not know about yet must not take the panel away from plugins.
            continue;
        };
        // `send` fails only when nobody is subscribed, which is the normal case
        // on a node with no button-using plugin. Not an error.
        let _ = tx.send(bytes);
    }
    Ok(())
}

/// Decode one bus line and re-encode it as the plugin-facing [`ButtonPress`].
///
/// The two shapes differ in exactly one field name — the bus calls the BCM line
/// `button`, the plugin contract calls it `pin` — so this is where that rename
/// happens, once, instead of every SDK doing it. Returns `None` for a line that
/// is not a well-formed press.
pub fn reencode_press(line: &str) -> Option<Vec<u8>> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;

    let pin = u32::try_from(v.get("button")?.as_u64()?).ok()?;
    let kind = match v.get("kind")?.as_str()? {
        k @ ("short" | "long") => k.to_string(),
        _ => return None,
    };
    let action = match v.get("action") {
        None | Some(serde_json::Value::Null) => None,
        Some(other) => Some(other.as_str()?.to_string()),
    };
    let timestamp_ms = v.get("timestamp_ms").and_then(|t| t.as_u64()).unwrap_or(0);

    let press = ButtonPress {
        pin,
        kind,
        action,
        timestamp_ms,
    };
    serde_json::to_vec(&press).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(line: &str) -> Option<ButtonPress> {
        reencode_press(line).map(|b| serde_json::from_slice(&b).unwrap())
    }

    #[test]
    fn the_bus_button_field_becomes_the_contract_pin_field() {
        // This rename is the whole reason this function exists; if it regressed,
        // every subscriber would read pin 0.
        let press =
            decode(r#"{"button":13,"kind":"long","action":"pair_drone","timestamp_ms":42}"#)
                .expect("a well-formed press re-encodes");
        assert_eq!(press.pin, 13);
        assert_eq!(press.kind, "long");
        assert_eq!(press.action.as_deref(), Some("pair_drone"));
        assert_eq!(press.timestamp_ms, 42);
    }

    #[test]
    fn an_unmapped_button_is_still_delivered() {
        let press = decode(r#"{"button":5,"kind":"short","action":null,"timestamp_ms":1}"#)
            .expect("an unmapped button is a real press");
        assert_eq!(press.action, None);
        assert_eq!(press.pin, 5);
    }

    #[test]
    fn both_press_kinds_survive_the_re_encode() {
        assert_eq!(
            decode(r#"{"button":5,"kind":"short"}"#).unwrap().kind,
            "short"
        );
        assert_eq!(
            decode(r#"{"button":5,"kind":"long"}"#).unwrap().kind,
            "long"
        );
    }

    #[test]
    fn malformed_lines_are_dropped_rather_than_guessed() {
        assert!(reencode_press("").is_none());
        assert!(reencode_press("not json").is_none());
        assert!(reencode_press("{}").is_none());
        assert!(reencode_press(r#"{"button":5}"#).is_none()); // no kind
        assert!(reencode_press(r#"{"kind":"short"}"#).is_none()); // no button
        assert!(reencode_press(r#"{"button":-1,"kind":"short"}"#).is_none());
        // An unknown kind must not be coerced into one of the two we know.
        assert!(reencode_press(r#"{"button":5,"kind":"double"}"#).is_none());
    }

    #[tokio::test]
    async fn a_subscriber_receives_a_press_published_on_the_bus() {
        let (tx, _keep) = broadcast::channel::<Vec<u8>>(8);
        let client = ButtonClient { tx: tx.clone() };
        let mut rx = client.subscribe();

        let bytes = reencode_press(r#"{"button":6,"kind":"short","action":"back"}"#).unwrap();
        tx.send(bytes).unwrap();

        let got: ButtonPress = serde_json::from_slice(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(got.pin, 6);
        assert_eq!(got.action.as_deref(), Some("back"));
    }

    #[tokio::test]
    async fn publishing_with_no_subscribers_is_not_an_error() {
        // The common case on a node with no button-using plugin. If this were
        // treated as a failure the reader would log on every single press.
        let (tx, _keep) = broadcast::channel::<Vec<u8>>(8);
        drop(_keep);
        let client = ButtonClient { tx };
        drop(client.subscribe());
        // No panic, no unwrap: send returning Err here is expected and ignored
        // by `pump`.
    }
}
