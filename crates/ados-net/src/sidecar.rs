//! Atomic tmp-sibling + rename writers for Contract-E sidecar files.
//!
//! Modeled on `ados-video/src/camera_state.rs`: write the body to a tmp
//! sibling, then `rename` over the destination so a reader never sees a
//! half-written file. Two flavors:
//!
//! * [`write_atomic`] — write + rename, no fsync. Matches the Python
//!   `save_priority` (`tmp.write_text(...)` then `os.replace`), which does not
//!   fsync. Used for the priority list and the active-uplink flag.
//! * [`write_atomic_fsync`] — write + `sync_all` + rename. For callers that
//!   want the bytes durable before the rename (later chunks may opt in).
//!
//! The tmp-suffix policy mirrors Python's `Path.with_suffix(".json.tmp")`,
//! which replaces the final `.json` component, so
//! `ground-station-uplink.json` → `ground-station-uplink.json.tmp`.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Convert a JSON object into the logging store's open detail map (`Fields`), so
/// a full uplink / data-cap snapshot can ride a single event. Recurses through
/// nested arrays / objects; numbers preserve their integer-vs-float kind, and
/// JSON null round-trips to msgpack nil. A non-object input yields an empty map
/// (every body here is an object). The map decodes back to the identical JSON in
/// the query path, so the store row is a faithful copy of the sidecar body.
pub(crate) fn json_object_to_fields(value: &serde_json::Value) -> ados_protocol::logd::Fields {
    match value {
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(k, v)| (k.clone(), json_to_mpv(v)))
            .collect(),
        _ => ados_protocol::logd::Fields::new(),
    }
}

/// Recursively map a `serde_json::Value` onto the msgpack value the detail map
/// carries. Integers stay integers (signed when negative), floats stay floats,
/// and null becomes nil, so the round-trip through the store preserves the exact
/// JSON shape the REST base merges over.
fn json_to_mpv(value: &serde_json::Value) -> ados_protocol::logd::Value {
    use ados_protocol::logd::Value as MpVal;
    match value {
        serde_json::Value::Null => MpVal::Nil,
        serde_json::Value::Bool(b) => MpVal::from(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                MpVal::from(i)
            } else if let Some(u) = n.as_u64() {
                MpVal::from(u)
            } else {
                MpVal::from(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => MpVal::from(s.as_str()),
        serde_json::Value::Array(items) => MpVal::Array(items.iter().map(json_to_mpv).collect()),
        serde_json::Value::Object(map) => MpVal::Map(
            map.iter()
                .map(|(k, v)| (MpVal::from(k.as_str()), json_to_mpv(v)))
                .collect(),
        ),
    }
}

/// Compute the tmp sibling for `path` the way Python `with_suffix(".json.tmp")`
/// does: replace a trailing `.json` extension with `.json.tmp`. For any other
/// (or absent) extension, append `.tmp` to the file name. The tmp file lives
/// in the same directory so the `rename` is same-filesystem and atomic.
pub fn tmp_sibling(path: &Path) -> PathBuf {
    match path.extension().and_then(|e| e.to_str()) {
        Some("json") => path.with_extension("json.tmp"),
        _ => {
            let mut name = path
                .file_name()
                .map(|n| n.to_os_string())
                .unwrap_or_default();
            name.push(".tmp");
            path.with_file_name(name)
        }
    }
}

/// Atomically write `body` to `path` (tmp sibling + rename), creating the
/// parent directory. No fsync, matching the Python `os.replace` path.
pub fn write_atomic(path: &Path, body: &[u8]) -> std::io::Result<()> {
    write_inner(path, body, false)
}

/// Atomically write `body` to `path` with an `fsync` of the tmp file before
/// the rename, so the payload is durable on disk before it becomes visible.
pub fn write_atomic_fsync(path: &Path, body: &[u8]) -> std::io::Result<()> {
    write_inner(path, body, true)
}

fn write_inner(path: &Path, body: &[u8], fsync: bool) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = tmp_sibling(path);
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(body)?;
        if fsync {
            f.sync_all()?;
        }
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmp_sibling_matches_python_with_suffix() {
        // `.json` → `.json.tmp` (replaces the trailing extension, not appends).
        assert_eq!(
            tmp_sibling(Path::new("/etc/ados/ground-station-uplink.json")),
            PathBuf::from("/etc/ados/ground-station-uplink.json.tmp")
        );
        // No `.json` extension → append `.tmp`.
        assert_eq!(
            tmp_sibling(Path::new("/run/ados/uplink-active")),
            PathBuf::from("/run/ados/uplink-active.tmp")
        );
    }

    #[test]
    fn write_atomic_round_trips_and_leaves_no_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ground-station-uplink.json");
        write_atomic(&path, br#"{"priority":["eth0"]}"#).unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            br#"{"priority":["eth0"]}"#.to_vec()
        );
        assert!(!dir.path().join("ground-station-uplink.json.tmp").exists());
    }

    #[test]
    fn write_atomic_fsync_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("uplink-active");
        write_atomic_fsync(&path, b"hello").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello".to_vec());
    }

    #[test]
    fn json_object_to_fields_round_trips_through_an_event_frame() {
        use ados_protocol::frame::{decode_len, HEADER_SIZE};
        use ados_protocol::logd::{EventFrame, IngestFrame, Level, LOGD_MAX_FRAME};

        // A body shaped like the uplink-active flag: a string, a bool, a u64, a
        // null, and a nested object — every kind the detail map carries.
        let body = serde_json::json!({
            "active_uplink": "eth0",
            "internet_reachable": true,
            "timestamp_ms": 1_700_000_000_000u64,
            "data_cap_state": "ok",
            "missing": serde_json::Value::Null,
            "nested": {"percent": 12.5, "used": 42},
        });
        let fields = json_object_to_fields(&body);
        let mut frame = EventFrame::new(0, "net.uplink_active", "ados-net", Level::Info);
        frame.detail = fields;
        let bytes = IngestFrame::Event(frame).encode().unwrap();
        let header: [u8; HEADER_SIZE] = bytes[..HEADER_SIZE].try_into().unwrap();
        let len = decode_len(header, LOGD_MAX_FRAME, true).unwrap();
        let decoded = match IngestFrame::decode(&bytes[HEADER_SIZE..HEADER_SIZE + len]).unwrap() {
            IngestFrame::Event(e) => e,
            other => panic!("expected an event frame, got {other:?}"),
        };
        let back = serde_json::to_value(decoded.detail).unwrap();
        assert_eq!(back, body);
        assert!(back["missing"].is_null());
        assert_eq!(back["nested"]["percent"], 12.5);
    }

    #[test]
    fn json_object_to_fields_of_a_non_object_is_empty() {
        assert!(json_object_to_fields(&serde_json::json!([1, 2, 3])).is_empty());
        assert!(json_object_to_fields(&serde_json::Value::Null).is_empty());
    }
}
