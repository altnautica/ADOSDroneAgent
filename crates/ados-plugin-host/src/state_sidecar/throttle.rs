//! Rate limit for the published-state sidecar.
//!
//! Every authorized publish used to rewrite the plugin's sidecar (load, merge,
//! cap, atomic replace). A plugin streaming a 30 Hz pose and a 10 Hz scan made
//! that a constant file rewrite, for a file the GCS reads every few seconds.
//! This writes each `(plugin, topic)` at most every [`MIN_INTERVAL`]: a publish
//! inside the window is held as the topic's pending value, and one trailing
//! write lands it when the window closes, so the last value published is always
//! the one the sidecar ends up holding.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use rmpv::Value;

/// The shortest gap between two sidecar writes of one topic (2 Hz).
pub const MIN_INTERVAL: Duration = Duration::from_millis(500);

type Key = (PathBuf, String, String);

#[derive(Default)]
struct Slot {
    last_write: Option<Instant>,
    pending: Option<(Value, i64)>,
    trailing: bool,
}

static SLOTS: LazyLock<Mutex<HashMap<Key, Slot>>> = LazyLock::new(Default::default);

fn slots() -> &'static Mutex<HashMap<Key, Slot>> {
    &SLOTS
}

fn write(key: &Key, payload: &Value, ts_ms: i64) {
    let (dir, plugin_id, topic) = key;
    if let Err(e) = super::record(dir, plugin_id, topic, payload, ts_ms) {
        tracing::warn!(plugin_id = %plugin_id, topic = %topic, error = %e, "failed to write plugin state sidecar");
    }
}

/// Record a publish into the sidecar, at most every [`MIN_INTERVAL`] per topic.
/// A publish inside the window replaces the topic's pending value; a single
/// trailing write lands the latest one when the window closes. Must run inside
/// a Tokio runtime (the trailing write is a spawned task).
pub fn record_throttled(
    socket_dir: &Path,
    plugin_id: &str,
    topic: &str,
    payload: &Value,
    ts_ms: i64,
) {
    let key: Key = (
        socket_dir.to_path_buf(),
        plugin_id.to_string(),
        topic.to_string(),
    );
    let now = Instant::now();
    let wait = {
        let mut map = slots().lock();
        let slot = map.entry(key.clone()).or_default();
        let since = slot.last_write.map(|t| now.saturating_duration_since(t));
        if !slot.trailing && since.is_none_or(|s| s >= MIN_INTERVAL) {
            slot.last_write = Some(now);
            None
        } else {
            slot.pending = Some((payload.clone(), ts_ms));
            if slot.trailing {
                return;
            }
            slot.trailing = true;
            Some(MIN_INTERVAL.saturating_sub(since.unwrap_or_default()))
        }
    };
    let Some(wait) = wait else {
        write(&key, payload, ts_ms);
        return;
    };
    tokio::spawn(async move {
        tokio::time::sleep(wait).await;
        let pending = {
            let mut map = slots().lock();
            // A plugin stopped meanwhile: its slots are gone and so is its
            // sidecar, which a late write must not recreate.
            let Some(slot) = map.get_mut(&key) else {
                return;
            };
            slot.trailing = false;
            slot.last_write = Some(Instant::now());
            slot.pending.take()
        };
        if let Some((payload, ts_ms)) = pending {
            write(&key, &payload, ts_ms);
        }
    });
}

/// Forget every topic of a plugin (on plugin stop), cancelling its trailing
/// writes.
pub(super) fn forget(socket_dir: &Path, plugin_id: &str) {
    slots()
        .lock()
        .retain(|(dir, id, _), _| !(dir == socket_dir && id == plugin_id));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(v: i64) -> Value {
        Value::Map(vec![(Value::from("n"), Value::from(v))])
    }

    fn sidecar_n(dir: &Path, topic: &str) -> Option<i64> {
        let text = std::fs::read_to_string(super::super::sidecar_path(dir, "demo")).ok()?;
        let doc: serde_json::Value = serde_json::from_str(&text).ok()?;
        doc[topic]["payload"]["n"].as_i64()
    }

    #[tokio::test]
    async fn a_burst_writes_the_first_now_and_the_last_after_the_window() {
        let dir = tempfile::tempdir().unwrap();
        for i in 1..=5 {
            record_throttled(dir.path(), "demo", "plugin.demo.pose", &n(i), i);
        }
        assert_eq!(
            sidecar_n(dir.path(), "plugin.demo.pose"),
            Some(1),
            "first write is immediate"
        );
        tokio::time::sleep(MIN_INTERVAL + Duration::from_millis(200)).await;
        assert_eq!(
            sidecar_n(dir.path(), "plugin.demo.pose"),
            Some(5),
            "the last value lands"
        );
    }

    #[tokio::test]
    async fn topics_are_throttled_independently() {
        let dir = tempfile::tempdir().unwrap();
        record_throttled(dir.path(), "demo", "a.state", &n(1), 1);
        record_throttled(dir.path(), "demo", "b.state", &n(2), 2);
        assert_eq!(sidecar_n(dir.path(), "a.state"), Some(1));
        assert_eq!(sidecar_n(dir.path(), "b.state"), Some(2));
    }

    #[tokio::test]
    async fn a_publish_after_the_window_writes_at_once() {
        let dir = tempfile::tempdir().unwrap();
        record_throttled(dir.path(), "demo", "follow.state", &n(1), 1);
        tokio::time::sleep(MIN_INTERVAL + Duration::from_millis(50)).await;
        record_throttled(dir.path(), "demo", "follow.state", &n(2), 2);
        assert_eq!(sidecar_n(dir.path(), "follow.state"), Some(2));
    }

    #[tokio::test]
    async fn a_stopped_plugin_gets_no_trailing_write() {
        let dir = tempfile::tempdir().unwrap();
        record_throttled(dir.path(), "demo", "x.state", &n(1), 1);
        record_throttled(dir.path(), "demo", "x.state", &n(2), 2);
        super::super::remove(dir.path(), "demo");
        tokio::time::sleep(MIN_INTERVAL + Duration::from_millis(200)).await;
        assert!(!super::super::sidecar_path(dir.path(), "demo").exists());
    }
}
