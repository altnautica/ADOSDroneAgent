//! Ground-station recordings media routes: the on-disk listing, the mediamtx
//! playback proxy, and the per-segment delete.
//!
//! - **`GET /api/v1/ground-station/recording/list`** — the recordings the ground
//!   node has captured to disk, newest first. Each entry carries `filename`,
//!   `size_bytes`, and `mtime` (Unix seconds, float). The envelope also carries
//!   the `recording` flag (a capture is in flight) and `current_filename` (the
//!   file the active capture is writing).
//! - **`GET /api/v1/ground-station/recording/segments?path=<p>`** — mediamtx's
//!   playback segment list for one stream path, relayed verbatim.
//! - **`GET /api/v1/ground-station/recording/clip?path=&start=&duration=`** —
//!   mediamtx's playback `/get`, which answers fMP4 a browser `<video>` plays
//!   directly. Relayed as a STREAM: a clip is a whole video file and buffering
//!   one in the front's memory to hand it on is a way to run a ground station
//!   out of RAM.
//! - **`DELETE /api/v1/ground-station/recording/<segment>`** — remove one
//!   segment file from the recordings directory.
//!
//! Every route is gated on the node resolving to the ground-station profile; a
//! drone-profile node answers the same `404`
//! `{"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}}` body the FastAPI
//! `_require_ground_profile` gate raises.
//!
//! ## Where the live recording flags come from
//!
//! The listing envelope's `recording` / `current_filename` used to be hardcoded
//! `false` / `null` on the stated grounds that "the native front has no
//! in-process recorder". That was false, and its own sibling proves it:
//! [`crate::routes::gs_recording`] holds the one `GroundStationRecorder` behind a
//! `OnceLock` because start and stop arrive as separate requests. So this
//! surface and `/status` answered differently about the same capture, one of them
//! known-false. Both now read
//! [`crate::routes::gs_recording::recording_view`] — one derivation, not a copy
//! per route.
//!
//! ## Recording is moving to mediamtx's native recorder
//!
//! mediamtx writes a continuous fMP4 segment stream under
//! `/var/ados/recordings/%path/<stamp>` and serves it back on its own playback
//! server on loopback `127.0.0.1:9996`. The playback routes here are a thin,
//! validated proxy onto that: this front already owns the LAN edge and the auth,
//! and exposing mediamtx's port directly would put an unauthenticated media
//! server on the network. The `list` read stays a plain directory scan, which is
//! what the on-demand recorder's flat captures need.

use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use http_body_util::Empty;
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpStream;

use ados_video::recorder::GroundStationRecorder;

use crate::state::AppState;

mod delete;
mod segments_clip;

pub use delete::delete_recording_segment;
#[cfg(test)]
use delete::*;
pub use segments_clip::get_recording_clip;
pub use segments_clip::get_recording_segments;
pub use segments_clip::ClipQuery;
pub use segments_clip::SegmentsQuery;
#[cfg(test)]
use segments_clip::*;

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

/// A `200` JSON body.
fn json_ok(body: Value) -> Response {
    (StatusCode::OK, Json(body)).into_response()
}

/// The FastAPI-shaped error body the sibling recording routes serve:
/// `{"detail": {"error": {"code", "message"}}}`. The same nested error-OBJECT
/// shape `gs_recording`'s recorder failures use, so a client parses ONE shape
/// across the whole recording surface rather than one per route.
fn error_body(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({"detail": {"error": {"code": code, "message": message}}})),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// The recordings directory seam.
// ---------------------------------------------------------------------------

/// The recordings directory the recorder writes captures to. The Python
/// `GroundStationRecorder` defaulted to `RECORDINGS_DIR` (`/var/ados/recordings`);
/// mediamtx's native recorder writes its segment tree under the same root. The
/// `ADOS_RECORDINGS_DIR` override is honoured here (tests redirect it at a
/// tempdir without touching the on-box path).
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
///
/// `tokio::fs`, not `std::fs`: this runs inside an axum handler, and a
/// blocking `read_dir` plus one blocking `stat` per entry over a day of segments
/// parks a runtime worker for the whole scan — which stalls every unrelated
/// route sharing that worker, not just this one.
async fn list_recordings(dir: &Path) -> Vec<Value> {
    let mut read_dir = match tokio::fs::read_dir(dir).await {
        Ok(rd) => rd,
        // Absent / unreadable directory → no recordings, never an error.
        Err(_) => return Vec::new(),
    };

    // Collect (mtime, item) so the newest-first sort can key on the raw mtime.
    let mut rows: Vec<(f64, Value)> = Vec::new();
    loop {
        let entry = match read_dir.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            // The directory stream itself faulted. Unlike a per-file stat
            // failure there is no next entry to advance to, so this ends the
            // listing with what was read rather than spinning on the same error.
            Err(_) => break,
        };
        let path = entry.path();
        if !has_mp4_extension(&path) {
            continue;
        }
        let meta = match entry.metadata().await {
            Ok(m) => m,
            // A stat failure skips the entry (the Python `except OSError`).
            Err(_) => continue,
        };
        // Regular files only: a directory named `x.mp4` is not a recording, and
        // neither is a symlink (`DirEntry::metadata` does not follow one), which
        // keeps the listing to files this directory actually holds.
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

/// True when `path`'s extension is `mp4` (case-insensitive), matching the Python
/// `entry.suffix.lower() == ".mp4"`. The `is_file()` half of the Python check is
/// applied at the call site off the metadata already read there, so the listing
/// costs one stat per entry rather than two.
fn has_mp4_extension(path: &Path) -> bool {
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

/// The `/recording/list` envelope: `{recording, current_filename, items}`.
///
/// The live flags come from [`crate::routes::gs_recording::recording_view`] —
/// the ONE derivation the `/status` recording legs also read — so the listing
/// surface and the status surface cannot disagree about whether a capture is in
/// flight. Split out from the handler so that agreement is testable against an
/// injected recorder, with no `AppState` and no process-wide singleton.
pub(crate) async fn recording_list_body(dir: &Path, recorder: &GroundStationRecorder) -> Value {
    let (recording, current_filename) = crate::routes::gs_recording::recording_view(recorder).await;
    json!({
        "recording": recording,
        "current_filename": current_filename,
        "items": list_recordings(dir).await,
    })
}

/// `GET /api/v1/ground-station/recording/list` → the recordings listing.
///
/// `404` `E_PROFILE_MISMATCH` off a drone-profile node. On a ground station,
/// returns `{recording, current_filename, items}`: the items are the on-disk
/// `.mp4` recordings (newest first), and the live flags follow the recorder this
/// binary owns. Guaranteed 200 on a ground-station node.
pub async fn get_recording_list(State(state): State<AppState>) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    let body =
        recording_list_body(&recordings_dir(), &crate::routes::gs_recording::recorder()).await;
    Json(body).into_response()
}

// ---------------------------------------------------------------------------
// The mediamtx playback server seam.
// ---------------------------------------------------------------------------

/// The loopback port mediamtx serves its playback API on (`/list`, `/get`).
///
/// Read from `ados-video`, which OWNS the mediamtx config that binds it: a
/// second literal here would be a port number in two crates, free to drift the
/// day one of them changes. mediamtx binds it to loopback precisely so the only
/// way in is through this authenticated front.
const PLAYBACK_PORT: u16 = ados_video::mediamtx::DEFAULT_PLAYBACK_PORT;

/// How long to wait for the loopback playback server to accept a connection.
/// mediamtx is on the same box, so a connect that does not complete promptly
/// means it is not running — which is a `503` for the operator, not a hang on
/// their request.
const PLAYBACK_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// The longest mediamtx stream path this surface forwards.
const MAX_STREAM_PATH: usize = 64;

/// The longest clip the playback proxy will fetch, in seconds.
///
/// An unbounded duration asks mediamtx to concatenate the whole retained day
/// into one response. An hour is longer than any single review a cockpit asks
/// for, and the bound keeps one request from monopolising the recordings
/// volume's read bandwidth while the recorder is writing to it.
const MAX_CLIP_DURATION_S: f64 = 3600.0;

/// The `503` a request gets when the playback server will not answer: mediamtx
/// is down, or this profile never started it.
fn playback_unavailable() -> Response {
    error_body(
        StatusCode::SERVICE_UNAVAILABLE,
        "E_PLAYBACK_UNAVAILABLE",
        "the mediamtx playback server is not answering on loopback",
    )
}

/// True when `path` is a mediamtx stream path this surface will forward: a
/// non-empty, bounded name of `[A-Za-z0-9._-]` with no `..`.
///
/// An allow-list, not a deny-list. This value is interpolated into the query
/// mediamtx resolves against the recordings tree, so anything that could read as
/// a path separator, a parent hop, or a request-line break is refused outright
/// rather than escaped and hoped about.
fn is_valid_stream_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= MAX_STREAM_PATH
        && !path.contains("..")
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// True when `value` parses as an RFC 3339 timestamp.
///
/// A real parse, not a shape check: mediamtx answers a `400` for a timestamp it
/// cannot read, and relaying that as if the recordings were the problem sends an
/// operator looking in the wrong place.
fn is_rfc3339(value: &str) -> bool {
    time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339).is_ok()
}

/// Percent-encode a query-parameter value down to the unreserved set.
///
/// Required, not decorative: an RFC 3339 timestamp carries `:` and, for a
/// non-UTC offset, `+` — and a raw `+` in a query decodes as a space, which
/// hands mediamtx a timestamp it cannot parse.
fn encode_query_value(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
    }
    out
}

/// One-shot HTTP/1.1 `GET` against the loopback playback server, returning the
/// upstream response with its body STILL STREAMING.
///
/// `hyper::client::conn::http1` over a raw `TcpStream` — the same one-shot
/// handshake the residual reverse proxy performs on its Unix socket, so the
/// crate carries no second HTTP client. Streaming is the point: a clip is a whole
/// video file, and a client that buffered it would hold the entire response in
/// the front's memory before the browser saw a frame.
async fn playback_get(
    port: u16,
    path_and_query: &str,
) -> Option<http::Response<hyper::body::Incoming>> {
    let connect = TcpStream::connect(("127.0.0.1", port));
    let stream = tokio::time::timeout(PLAYBACK_CONNECT_TIMEOUT, connect)
        .await
        .ok()?
        .ok()?;
    let (mut sender, conn) = tokio::time::timeout(
        PLAYBACK_HEAD_TIMEOUT,
        hyper::client::conn::http1::handshake(TokioIo::new(stream)),
    )
    .await
    .ok()?
    .ok()?;
    // The connection future must be polled for the exchange to progress. It ends
    // when the body is fully read or the peer closes, neither of which is a fault.
    let conn_task = tokio::spawn(async move {
        let _ = conn.await;
    });
    let request = http::Request::builder()
        .method(http::Method::GET)
        .uri(path_and_query)
        .header(http::header::HOST, format!("127.0.0.1:{port}"))
        .body(Empty::<Bytes>::new())
        .ok()?;
    // The response HEAD is bounded; the clip body that follows streams for as
    // long as it takes. A server that accepts and never answers is unavailable.
    match tokio::time::timeout(PLAYBACK_HEAD_TIMEOUT, sender.send_request(request)).await {
        Ok(r) => r.ok(),
        Err(_) => {
            conn_task.abort();
            None
        }
    }
}

/// How long the playback server has to send a response head. A segment list or
/// the start of a clip is served from local disk in milliseconds.
const PLAYBACK_HEAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[cfg(test)]
mod tests;
