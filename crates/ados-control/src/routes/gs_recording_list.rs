//! Ground-station recordings listing read route.
//!
//! - **`GET /api/v1/ground-station/recording/list`** — the recordings the ground
//!   node has captured to disk, newest first. Each entry carries `filename`,
//!   `size_bytes`, and `mtime` (Unix seconds, float). The envelope also carries
//!   the `recording` flag (a capture is in flight) and `current_filename` (the
//!   file the active capture is writing). Gated on the node resolving to the
//!   ground-station profile; a drone-profile node answers the same `404`
//!   `{"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}}` body the FastAPI
//!   `_require_ground_profile` gate raises.
//!
//! The FastAPI route reads the recordings off the process-wide
//! `GroundStationRecorder` singleton: `is_active()` for the flag,
//! `current_filename` for the in-flight name, and `list_recordings()` for the
//! items. `list_recordings()` is itself a plain directory listing — it enumerates
//! `.mp4` files in the recordings directory (`/var/ados/recordings`) and reports
//! each file's name, size, and mtime, sorted newest-first by mtime.
//!
//! The native front runs the recorder in a sibling service (no in-process
//! recorder to call), so it reads the items from the SAME on-disk directory the
//! recorder writes to, and degrades the live flags to the inactive shape — the
//! exact shape the FastAPI route returns when no capture is in flight
//! (`recording = false`, `current_filename = null`). This mirrors how the
//! `/status` route's recorder block degrades to inactive on the native front.

use std::path::{Path, PathBuf};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::state::AppState;

// ---------------------------------------------------------------------------
// Profile gate (the same shape gs_status.rs emits).
// ---------------------------------------------------------------------------

/// True when the node resolves to the ground-station profile, via
/// `current_profile_and_role` (the same source of truth the node advertises on
/// the wire), so a `profile: auto` node that resolves to a ground station via
/// `profile.conf` passes the gate, matching the Python `_require_ground_profile`.
fn is_ground_station(state: &AppState) -> bool {
    let cfg = crate::config::PairingConfig::load_from(&state.pairing_paths.config);
    let (profile, _role) = crate::profile::current_profile_and_role(&cfg.agent.profile);
    profile == "ground-station"
}

/// The `404` profile-mismatch response, byte-identical to the FastAPI
/// `HTTPException(status_code=404, detail={"error": {"code": "E_PROFILE_MISMATCH"}})`
/// (FastAPI wraps the `detail` dict under a top-level `"detail"` key).
fn profile_mismatch() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// The recordings directory seam.
// ---------------------------------------------------------------------------

/// The recordings directory the recorder writes `.mp4` captures to. The Python
/// `GroundStationRecorder` defaults to `RECORDINGS_DIR` (`/var/ados/recordings`);
/// the native front reads the same directory. `ADOS_RECORDINGS_DIR` overrides it
/// (tests redirect it at a tempdir without touching the on-box path).
fn recordings_dir() -> PathBuf {
    PathBuf::from(
        std::env::var("ADOS_RECORDINGS_DIR").unwrap_or_else(|_| "/var/ados/recordings".to_string()),
    )
}

/// Enumerate the `.mp4` files in the recordings directory as the `items` array the
/// `/recording/list` route returns, newest-first by mtime. Each entry is
/// `{filename, size_bytes, mtime}` (mtime in Unix seconds, float), mirroring the
/// Python `RecordingFile.to_dict()`. An absent directory (a fresh ground station
/// that has never recorded) yields the empty list, matching the Python
/// `if not self._dir.is_dir(): return []`. A per-file stat failure skips that file,
/// matching the Python `except OSError: continue`.
fn list_recordings(dir: &Path) -> Vec<Value> {
    let read_dir = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        // Absent / unreadable directory → no recordings, never an error.
        Err(_) => return Vec::new(),
    };

    // Collect (mtime, item) so the newest-first sort can key on the raw mtime.
    let mut rows: Vec<(f64, Value)> = Vec::new();
    for entry in read_dir.flatten() {
        let path = entry.path();
        // Only regular `.mp4` files; the Python checks `is_file()` and a
        // case-insensitive `.mp4` suffix.
        if !is_mp4_file(&path) {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            // A stat failure skips the entry (the Python `except OSError`).
            Err(_) => continue,
        };
        // `is_file()` filtered above, but a metadata read can still race; keep the
        // file check tight so a directory named `x.mp4` never lands in the list.
        if !meta.is_file() {
            continue;
        }
        let filename = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let size_bytes = meta.len() as i64;
        let mtime = mtime_secs(&meta);
        rows.push((
            mtime,
            json!({
                "filename": filename,
                "size_bytes": size_bytes,
                "mtime": mtime,
            }),
        ));
    }

    // Newest first by mtime. The Python `sort(key=mtime, reverse=True)` is stable;
    // a stable descending sort matches its tie ordering for equal mtimes.
    rows.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    rows.into_iter().map(|(_, item)| item).collect()
}

/// True when `path` is a regular file whose extension is `mp4` (case-insensitive),
/// matching the Python `entry.is_file() and entry.suffix.lower() == ".mp4"`.
fn is_mp4_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("mp4"))
        .unwrap_or(false)
}

/// A file's mtime in Unix seconds as an `f64`, matching the Python `st_mtime`
/// (seconds since the epoch, sub-second precision). A clock before the epoch or an
/// unreadable mtime degrades to `0.0` (a stat that fails is already filtered out
/// upstream; this only guards the conversion).
fn mtime_secs(meta: &std::fs::Metadata) -> f64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/recording/list
// ---------------------------------------------------------------------------

/// `GET /api/v1/ground-station/recording/list` → the recordings listing.
///
/// `404` `E_PROFILE_MISMATCH` off a drone-profile node. On a ground station,
/// returns `{recording, current_filename, items}`: the items are the on-disk
/// `.mp4` recordings (newest first), and the live flags degrade to the inactive
/// shape (`recording = false`, `current_filename = null`) because the native
/// front has no in-process recorder — the same shape the FastAPI route returns
/// when no capture is in flight. Guaranteed 200 on a ground-station node.
pub async fn get_recording_list(State(state): State<AppState>) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }

    let items = list_recordings(&recordings_dir());

    Json(json!({
        "recording": false,
        "current_filename": Value::Null,
        "items": items,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    #[test]
    fn profile_mismatch_body_is_the_fastapi_404_shape() {
        let resp = profile_mismatch();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        // The body shape is the contract; build it independently and compare.
        let want = json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}});
        assert_eq!(
            want,
            json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})
        );
    }

    #[test]
    fn list_of_an_absent_dir_is_empty() {
        // A fresh ground station that has never recorded has no recordings dir;
        // the list is empty (never an error), matching the Python `is_dir()` guard.
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("never-created");
        assert_eq!(list_recordings(&absent), Vec::<Value>::new());
    }

    #[test]
    fn list_of_an_empty_dir_is_empty() {
        // An existing-but-empty recordings dir also yields the empty list.
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(list_recordings(dir.path()), Vec::<Value>::new());
    }

    #[test]
    fn list_enumerates_mp4_files_with_the_three_fields() {
        // Two .mp4 files land in the list, each with filename + size_bytes + mtime;
        // a non-.mp4 file and a subdirectory are both excluded.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.mp4"), b"aaaa").unwrap();
        fs::write(dir.path().join("b.mp4"), b"bbbbbbbb").unwrap();
        fs::write(dir.path().join("notes.txt"), b"x").unwrap();
        fs::create_dir(dir.path().join("subdir.mp4")).unwrap();

        let items = list_recordings(dir.path());
        assert_eq!(items.len(), 2);
        for item in &items {
            let obj = item.as_object().unwrap();
            // Exactly the three contract fields, no more.
            assert_eq!(obj.len(), 3);
            assert!(obj.get("filename").and_then(Value::as_str).is_some());
            assert!(obj.get("size_bytes").and_then(Value::as_i64).is_some());
            assert!(obj.get("mtime").and_then(Value::as_f64).is_some());
        }
        let names: Vec<&str> = items
            .iter()
            .filter_map(|i| i.get("filename").and_then(Value::as_str))
            .collect();
        assert!(names.contains(&"a.mp4"));
        assert!(names.contains(&"b.mp4"));
        assert!(!names.contains(&"notes.txt"));
        assert!(!names.contains(&"subdir.mp4"));
    }

    #[test]
    fn list_reports_size_bytes_from_the_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("clip.mp4"), b"0123456789").unwrap();
        let items = list_recordings(dir.path());
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["filename"], json!("clip.mp4"));
        assert_eq!(items[0]["size_bytes"], json!(10));
    }

    #[test]
    fn list_is_newest_first_by_mtime() {
        // Two files with explicit, distinct mtimes must come back newest-first.
        use std::time::{Duration, SystemTime};
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.mp4");
        let new = dir.path().join("new.mp4");
        fs::write(&old, b"o").unwrap();
        fs::write(&new, b"n").unwrap();

        let base = SystemTime::now();
        let old_time = base - Duration::from_secs(100);
        let new_time = base;
        filetime_set(&old, old_time);
        filetime_set(&new, new_time);

        let items = list_recordings(dir.path());
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["filename"], json!("new.mp4"));
        assert_eq!(items[1]["filename"], json!("old.mp4"));
    }

    #[test]
    fn mp4_suffix_match_is_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("upper.MP4"), b"x").unwrap();
        fs::write(dir.path().join("mixed.Mp4"), b"x").unwrap();
        let items = list_recordings(dir.path());
        assert_eq!(items.len(), 2);
    }

    /// Set a file's mtime directly so the newest-first ordering test is
    /// deterministic, without depending on filesystem write-order timing.
    fn filetime_set(path: &Path, when: std::time::SystemTime) {
        // `File::set_modified` sets the mtime directly (no extra dependency).
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(when).unwrap();
    }
}
