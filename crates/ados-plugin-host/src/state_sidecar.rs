//! Per-plugin published-state sidecar.
//!
//! A plugin publishes its own state on its own namespaced topics (a
//! `follow.state`-style read-back, gated by the `event.publish` capability and
//! the per-topic publish check). That event reaches the in-process event bus and
//! fans out to other plugins on-box, but nothing carries it off-box for the
//! operator's ground station to read. This module is that carrier: on every
//! successful authorized publish it writes the LATEST event per topic into a
//! small JSON sidecar at `<socket_dir>/<plugin_id>-state.json`, which the native
//! control front reads and the GCS polls.
//!
//! The sidecar holds a map `{ "<topic>": { "payload": <json>, "ts_ms": <int> } }`
//! keyed by topic, capped to a small number of topics and a small byte budget so
//! a chatty plugin cannot grow the file without bound. The publish authorization
//! (a plugin can only emit topics it was granted) is the security boundary: the
//! operator installed the plugin and approved its `event.publish` capability, so
//! exposing the plugin's own published events to the operator's GCS is correct.
//!
//! The write is a load → merge → cap → atomic-replace each time, so the latest
//! state survives a plugin reconnect (the merge reads the prior sidecar back in)
//! and a partial write never leaves a torn file (temp + rename). At the slow
//! cadence a state read-back is published this is cheap; the cap bounds the work.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rmpv::Value;
use serde_json::{json, Map, Value as Json};

/// Maximum distinct topics retained in one plugin's sidecar. A plugin that
/// publishes more than this keeps the most-recently-updated topics (the oldest
/// by last-update timestamp is evicted), so the file stays bounded.
pub const MAX_TOPICS: usize = 8;

/// Maximum serialized sidecar size in bytes. A single event whose serialized
/// form alone exceeds this is refused (the prior sidecar is left untouched); an
/// accumulated file over budget evicts oldest topics until it fits.
pub const MAX_BYTES: usize = 64 * 1024;

/// The sidecar file path for a plugin id under a socket dir:
/// `<socket_dir>/<plugin_id>-state.json`. The `-state.json` suffix keeps it
/// distinct from the plugin's `<plugin_id>.sock` and `<plugin_id>.token.env`.
pub fn sidecar_path(socket_dir: &Path, plugin_id: &str) -> PathBuf {
    socket_dir.join(format!("{plugin_id}-state.json"))
}

/// One retained entry: the latest payload for a topic and the wall-clock ms it
/// was published.
#[derive(Debug, Clone, PartialEq)]
struct Entry {
    payload: Json,
    ts_ms: i64,
}

/// Record the latest event for `topic` into the plugin's sidecar.
///
/// Loads the current sidecar (an absent / unreadable / malformed file starts
/// from empty), inserts/overwrites the topic with `payload` + `ts_ms`, caps the
/// map to [`MAX_TOPICS`] and [`MAX_BYTES`] (evicting the oldest-updated topics),
/// then atomically replaces the file. Best-effort: a write fault is returned but
/// is never fatal to the publish (the caller logs and continues).
///
/// `payload` is the plugin's msgpack event payload; it is converted to JSON for
/// the file. A plugin payload is a JSON object on the wire (string-keyed map), so
/// the conversion is lossless for the shapes plugins publish.
pub fn record(
    socket_dir: &Path,
    plugin_id: &str,
    topic: &str,
    payload: &Value,
    ts_ms: i64,
) -> std::io::Result<()> {
    let path = sidecar_path(socket_dir, plugin_id);
    let mut entries = load_entries(&path);

    let payload_json = msgpack_to_json(payload);
    // Refuse a single oversize event outright rather than churning the file: if
    // even this one entry serialized exceeds the budget, leave the prior sidecar.
    if estimate_one(topic, &payload_json) > MAX_BYTES {
        return Ok(());
    }

    entries.insert(
        topic.to_string(),
        Entry {
            payload: payload_json,
            ts_ms,
        },
    );
    cap(&mut entries);

    let body = serialize(&entries);
    write_atomic(&path, body.as_bytes())
}

/// Remove a plugin's sidecar (on plugin stop). Best-effort: an absent file is a
/// no-op; an unlink fault is swallowed so a stop path never fails on it.
pub fn remove(socket_dir: &Path, plugin_id: &str) {
    let _ = std::fs::remove_file(sidecar_path(socket_dir, plugin_id));
}

/// Load the sidecar entries from disk, degrading an absent / unreadable /
/// malformed file to an empty map (the sidecar is a cache, never a source of
/// truth, so a corrupt file is simply rebuilt on the next publish).
fn load_entries(path: &Path) -> BTreeMap<String, Entry> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return BTreeMap::new(),
    };
    let doc: Json = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return BTreeMap::new(),
    };
    let Some(obj) = doc.as_object() else {
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    for (topic, entry) in obj {
        let Some(map) = entry.as_object() else {
            continue;
        };
        let Some(payload) = map.get("payload").cloned() else {
            continue;
        };
        let ts_ms = map.get("ts_ms").and_then(Json::as_i64).unwrap_or(0);
        out.insert(topic.clone(), Entry { payload, ts_ms });
    }
    out
}

/// Cap the entry map to [`MAX_TOPICS`] then to [`MAX_BYTES`], evicting the
/// oldest-updated topic each round (the smallest `ts_ms`, ties broken by topic
/// name so eviction is deterministic). The just-inserted topic has the newest
/// `ts_ms`, so it is never the one evicted.
fn cap(entries: &mut BTreeMap<String, Entry>) {
    while entries.len() > MAX_TOPICS {
        if let Some(victim) = oldest_topic(entries) {
            entries.remove(&victim);
        } else {
            break;
        }
    }
    while serialize(entries).len() > MAX_BYTES && entries.len() > 1 {
        if let Some(victim) = oldest_topic(entries) {
            entries.remove(&victim);
        } else {
            break;
        }
    }
}

/// The topic with the smallest `ts_ms` (ties broken by topic name), or `None`
/// when the map is empty.
fn oldest_topic(entries: &BTreeMap<String, Entry>) -> Option<String> {
    entries
        .iter()
        .min_by(|(at, ae), (bt, be)| ae.ts_ms.cmp(&be.ts_ms).then_with(|| at.cmp(bt)))
        .map(|(topic, _)| topic.clone())
}

/// Serialize the entry map to the sidecar JSON shape: a top-level object keyed
/// by topic, each value `{ "payload": <json>, "ts_ms": <int> }`. The keys are
/// `BTreeMap`-ordered, which is stable across writes (no spurious file churn).
fn serialize(entries: &BTreeMap<String, Entry>) -> String {
    let mut obj = Map::new();
    for (topic, entry) in entries {
        obj.insert(
            topic.clone(),
            json!({ "payload": entry.payload, "ts_ms": entry.ts_ms }),
        );
    }
    serde_json::to_string(&Json::Object(obj)).unwrap_or_else(|_| "{}".to_string())
}

/// A rough serialized-size estimate for a single `{topic: {payload, ts_ms}}`
/// entry, used to refuse an oversize single event before it touches the file.
fn estimate_one(topic: &str, payload: &Json) -> usize {
    let one: BTreeMap<String, Entry> = std::iter::once((
        topic.to_string(),
        Entry {
            payload: payload.clone(),
            ts_ms: 0,
        },
    ))
    .collect();
    serialize(&one).len()
}

/// Convert a msgpack [`Value`] to a [`serde_json::Value`]. A plugin event
/// payload is a string-keyed map on the wire, so this is lossless for the
/// published shapes; the few edge cases (a non-string map key, a binary blob)
/// degrade to a stable JSON form rather than failing, so a malformed payload can
/// never break the sidecar write.
fn msgpack_to_json(value: &Value) -> Json {
    match value {
        Value::Nil => Json::Null,
        Value::Boolean(b) => Json::Bool(*b),
        Value::Integer(i) => {
            if let Some(u) = i.as_u64() {
                Json::from(u)
            } else if let Some(n) = i.as_i64() {
                Json::from(n)
            } else if let Some(f) = i.as_f64() {
                serde_json::Number::from_f64(f)
                    .map(Json::Number)
                    .unwrap_or(Json::Null)
            } else {
                Json::Null
            }
        }
        Value::F32(f) => serde_json::Number::from_f64(*f as f64)
            .map(Json::Number)
            .unwrap_or(Json::Null),
        Value::F64(f) => serde_json::Number::from_f64(*f)
            .map(Json::Number)
            .unwrap_or(Json::Null),
        Value::String(s) => Json::String(s.as_str().unwrap_or("").to_string()),
        Value::Binary(b) => Json::String(general_purpose_b64(b)),
        Value::Array(items) => Json::Array(items.iter().map(msgpack_to_json).collect()),
        Value::Map(entries) => {
            let mut obj = Map::new();
            for (k, v) in entries {
                // A non-string key is rendered to its display form so the entry
                // is never silently dropped (plugin payloads use string keys).
                let key = match k {
                    Value::String(s) => s.as_str().unwrap_or("").to_string(),
                    other => other.to_string(),
                };
                obj.insert(key, msgpack_to_json(v));
            }
            Json::Object(obj)
        }
        Value::Ext(_, data) => Json::String(general_purpose_b64(data)),
    }
}

/// Standard base64 (no padding stripped) for a binary blob, so a binary value in
/// a payload becomes a readable string rather than being dropped.
fn general_purpose_b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Write `bytes` to `path` atomically: ensure the parent dir, write a sibling
/// `.tmp`, set its mode to 0o644 (world-readable so the front, running as the
/// agent user, can read a sidecar another plugin process wrote), then rename
/// over the target. The same temp + rename idiom the rest of the agent uses.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    set_mode_644(&tmp)?;
    std::fs::rename(&tmp, path)
}

/// Set a file's mode to 0o644. Linux-only; a no-op elsewhere so the core builds
/// and tests on a non-Linux dev host.
#[cfg(target_os = "linux")]
fn set_mode_644(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))
}

#[cfg(not(target_os = "linux"))]
fn set_mode_644(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_doc(path: &Path) -> Json {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn follow_payload(active: bool, lock: &str) -> Value {
        Value::Map(vec![
            (Value::from("active"), Value::Boolean(active)),
            (Value::from("lock_state"), Value::from(lock)),
            (
                Value::from("commanding"),
                Value::Boolean(active && lock == "locked"),
            ),
        ])
    }

    #[test]
    fn records_latest_per_topic() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        record(
            p,
            "demo",
            "follow.state",
            &follow_payload(true, "uncertain"),
            10,
        )
        .unwrap();
        record(
            p,
            "demo",
            "follow.state",
            &follow_payload(true, "locked"),
            20,
        )
        .unwrap();

        let doc = read_doc(&sidecar_path(p, "demo"));
        let entry = &doc["follow.state"];
        assert_eq!(entry["ts_ms"], json!(20));
        // The latest event won — locked, commanding true.
        assert_eq!(entry["payload"]["lock_state"], json!("locked"));
        assert_eq!(entry["payload"]["commanding"], json!(true));
        // Only one topic is present.
        assert_eq!(doc.as_object().unwrap().len(), 1);
    }

    #[test]
    fn keeps_multiple_topics_under_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        record(p, "demo", "plugin.demo.a", &Value::from(1), 1).unwrap();
        record(p, "demo", "plugin.demo.b", &Value::from(2), 2).unwrap();
        let doc = read_doc(&sidecar_path(p, "demo"));
        assert_eq!(doc.as_object().unwrap().len(), 2);
        assert_eq!(doc["plugin.demo.a"]["payload"], json!(1));
        assert_eq!(doc["plugin.demo.b"]["payload"], json!(2));
    }

    #[test]
    fn evicts_the_oldest_topic_past_the_topic_cap() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        // Fill exactly MAX_TOPICS with ascending timestamps, then add one more.
        for i in 0..MAX_TOPICS {
            record(
                p,
                "demo",
                &format!("t{i}"),
                &Value::from(i as i64),
                i as i64,
            )
            .unwrap();
        }
        record(p, "demo", "t_new", &Value::from(99), 999).unwrap();
        let doc = read_doc(&sidecar_path(p, "demo"));
        let obj = doc.as_object().unwrap();
        assert_eq!(obj.len(), MAX_TOPICS, "stays capped at MAX_TOPICS");
        // The oldest (t0, ts 0) was evicted; the newest is present.
        assert!(!obj.contains_key("t0"), "oldest topic evicted");
        assert!(obj.contains_key("t_new"), "newest topic kept");
    }

    #[test]
    fn the_just_recorded_topic_is_never_evicted() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        for i in 0..(MAX_TOPICS + 3) {
            let topic = format!("t{i}");
            record(p, "demo", &topic, &Value::from(i as i64), i as i64).unwrap();
            let doc = read_doc(&sidecar_path(p, "demo"));
            assert!(
                doc.as_object().unwrap().contains_key(&topic),
                "the topic just written must always be present"
            );
        }
    }

    #[test]
    fn atomic_write_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        record(
            p,
            "demo",
            "follow.state",
            &follow_payload(true, "locked"),
            5,
        )
        .unwrap();
        let tmp = sidecar_path(p, "demo").with_extension("json.tmp");
        assert!(!tmp.exists(), "the temp file must be renamed away");
        assert!(sidecar_path(p, "demo").exists());
    }

    #[test]
    fn malformed_existing_file_is_rebuilt_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::write(sidecar_path(p, "demo"), b"not json {{{").unwrap();
        record(p, "demo", "follow.state", &follow_payload(false, "lost"), 7).unwrap();
        let doc = read_doc(&sidecar_path(p, "demo"));
        assert_eq!(doc["follow.state"]["ts_ms"], json!(7));
    }

    #[test]
    fn remove_unlinks_the_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        record(
            p,
            "demo",
            "follow.state",
            &follow_payload(true, "locked"),
            1,
        )
        .unwrap();
        assert!(sidecar_path(p, "demo").exists());
        remove(p, "demo");
        assert!(!sidecar_path(p, "demo").exists());
        // A second remove on an absent file is a no-op.
        remove(p, "demo");
    }

    #[test]
    fn an_oversize_single_event_is_refused_without_touching_the_prior_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        record(
            p,
            "demo",
            "follow.state",
            &follow_payload(true, "locked"),
            1,
        )
        .unwrap();
        let before = std::fs::read_to_string(sidecar_path(p, "demo")).unwrap();
        // A payload whose serialized form alone exceeds MAX_BYTES.
        let huge = Value::String("x".repeat(MAX_BYTES + 10).into());
        record(p, "demo", "plugin.demo.huge", &huge, 2).unwrap();
        let after = std::fs::read_to_string(sidecar_path(p, "demo")).unwrap();
        assert_eq!(before, after, "the prior sidecar is left untouched");
    }

    #[test]
    fn nested_payload_round_trips_to_json() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let payload = Value::Map(vec![
            (Value::from("active"), Value::Boolean(true)),
            (Value::from("range_m"), Value::F64(7.5)),
            (Value::from("target_id"), Value::from(42)),
            (
                Value::from("nested"),
                Value::Array(vec![Value::from(1), Value::from(2)]),
            ),
        ]);
        record(p, "demo", "follow.state", &payload, 3).unwrap();
        let doc = read_doc(&sidecar_path(p, "demo"));
        let pl = &doc["follow.state"]["payload"];
        assert_eq!(pl["active"], json!(true));
        assert_eq!(pl["range_m"], json!(7.5));
        assert_eq!(pl["target_id"], json!(42));
        assert_eq!(pl["nested"], json!([1, 2]));
    }
}
