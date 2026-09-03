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
//! known-false (rule 6). Both now read
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
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .ok()?;
    // The connection future must be polled for the exchange to progress. It ends
    // when the body is fully read or the peer closes, neither of which is a fault.
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let request = http::Request::builder()
        .method(http::Method::GET)
        .uri(path_and_query)
        .header(http::header::HOST, format!("127.0.0.1:{port}"))
        .body(Empty::<Bytes>::new())
        .ok()?;
    sender.send_request(request).await.ok()
}

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/recording/segments
// ---------------------------------------------------------------------------

/// The `/recording/segments` query. `path` is optional at the type level so a
/// missing parameter is refused with THIS surface's error shape rather than
/// axum's own plain-text rejection.
#[derive(Debug, Default, Deserialize)]
pub struct SegmentsQuery {
    #[serde(default)]
    pub path: Option<String>,
}

/// `GET .../recording/segments?path=<p>` → mediamtx's playback segment list.
///
/// `404` `E_PROFILE_MISMATCH` off a drone-profile node, `400` `E_INVALID_PATH`
/// for a stream path outside the allow-list, `503` `E_PLAYBACK_UNAVAILABLE` when
/// mediamtx will not answer. Otherwise mediamtx's own response is relayed
/// verbatim — it is the authority on what it has recorded, and reshaping its
/// answer here would be a second inventory to drift from the first.
pub async fn get_recording_segments(
    State(state): State<AppState>,
    Query(query): Query<SegmentsQuery>,
) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    let path = query.path.as_deref().unwrap_or_default();
    if !is_valid_stream_path(path) {
        return error_body(
            StatusCode::BAD_REQUEST,
            "E_INVALID_PATH",
            "path must be a stream name of letters, digits, '.', '_' or '-'",
        );
    }
    let target = format!("/list?path={}", encode_query_value(path));
    match playback_get(PLAYBACK_PORT, &target).await {
        Some(upstream) => crate::proxy::relay_response(upstream),
        None => playback_unavailable(),
    }
}

// ---------------------------------------------------------------------------
// GET /api/v1/ground-station/recording/clip
// ---------------------------------------------------------------------------

/// The `/recording/clip` query. Every field is optional-and-textual at the type
/// level so each rejection carries this surface's error shape and a code that
/// says WHICH parameter was wrong; `duration` in particular must distinguish
/// "unparseable" from "out of range".
#[derive(Debug, Default, Deserialize)]
pub struct ClipQuery {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub start: Option<String>,
    #[serde(default)]
    pub duration: Option<String>,
}

/// Why a `/clip` query cannot be forwarded: the stable code and the message the
/// rejection carries. A code + message rather than a whole built `Response`,
/// because every clip rejection is the same `400` and returning a response
/// inside an error variant makes the success path pay for it on every call.
#[derive(Debug)]
struct ClipReject {
    code: &'static str,
    message: &'static str,
}

/// Validate and bound the `/clip` query, returning the triple to forward or why
/// it is refused. Every parameter is checked: an unparseable `start` and a
/// non-positive or absurd `duration` are refused here rather than forwarded for
/// mediamtx to answer about.
fn validated_clip(query: &ClipQuery) -> Result<(String, String, f64), ClipReject> {
    let path = query.path.as_deref().unwrap_or_default();
    if !is_valid_stream_path(path) {
        return Err(ClipReject {
            code: "E_INVALID_PATH",
            message: "path must be a stream name of letters, digits, '.', '_' or '-'",
        });
    }
    let start = query.start.as_deref().unwrap_or_default();
    if !is_rfc3339(start) {
        return Err(ClipReject {
            code: "E_INVALID_START",
            message: "start must be an RFC 3339 timestamp",
        });
    }
    let duration = match query.duration.as_deref().unwrap_or_default().parse::<f64>() {
        Ok(d) => d,
        Err(_) => {
            return Err(ClipReject {
                code: "E_INVALID_DURATION",
                message: "duration must be a number of seconds",
            })
        }
    };
    if !duration.is_finite() || duration <= 0.0 || duration > MAX_CLIP_DURATION_S {
        return Err(ClipReject {
            code: "E_INVALID_DURATION",
            message: "duration must be greater than 0 and at most 3600 seconds",
        });
    }
    Ok((path.to_string(), start.to_string(), duration))
}

/// `GET .../recording/clip?path=&start=&duration=` → the fMP4 clip mediamtx cuts
/// from its recorded segments, relayed as a stream so a browser `<video>` plays
/// it directly.
///
/// `404` `E_PROFILE_MISMATCH` off a drone-profile node; `400`
/// `E_INVALID_PATH` / `E_INVALID_START` / `E_INVALID_DURATION` for a parameter
/// that does not validate; `503` `E_PLAYBACK_UNAVAILABLE` when mediamtx will not
/// answer.
pub async fn get_recording_clip(
    State(state): State<AppState>,
    Query(query): Query<ClipQuery>,
) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    let (path, start, duration) = match validated_clip(&query) {
        Ok(triple) => triple,
        Err(reject) => return error_body(StatusCode::BAD_REQUEST, reject.code, reject.message),
    };
    let target = format!(
        "/get?path={}&start={}&duration={}",
        encode_query_value(&path),
        encode_query_value(&start),
        duration
    );
    match playback_get(PLAYBACK_PORT, &target).await {
        Some(upstream) => crate::proxy::relay_response(upstream),
        None => playback_unavailable(),
    }
}

// ---------------------------------------------------------------------------
// DELETE /api/v1/ground-station/recording/<segment>
// ---------------------------------------------------------------------------

/// The longest segment file name this surface will accept. mediamtx's own
/// `<stamp>` names are around 30 characters; this is generous and still bounds
/// the value before it reaches the filesystem.
const MAX_SEGMENT_NAME: usize = 128;

/// Why a `DELETE .../recording/<segment>` names nothing this surface may remove.
#[derive(Debug, PartialEq, Eq)]
enum SegmentReject {
    /// The name is not one this surface accepts — a `400`, never a `404`: it is
    /// the request that is wrong, and answering `404` would tell an operator the
    /// file is missing when the name never could have named a file.
    Invalid,
    /// The name is fine but resolves to nothing removable.
    Absent,
}

/// True when `segment` names one entry directly inside the recordings directory:
/// non-empty, bounded, no path separator, no parent hop, not a bare `.` or `..`.
///
/// The FIRST of two gates. Path traversal is THE hazard on a DELETE that takes
/// its target from the URL — axum percent-decodes the path parameter, so `%2e%2e%2f`
/// arrives here as `../` — and a name check alone is not enough, because a
/// symlink planted inside the recordings directory pointing at
/// `/etc/ados/config.yaml` passes every string test there is.
fn is_plain_segment_name(segment: &str) -> bool {
    if segment.is_empty() || segment.len() > MAX_SEGMENT_NAME {
        return false;
    }
    if segment.contains('/') || segment.contains('\\') || segment.contains('\0') {
        return false;
    }
    if segment.contains("..") {
        return false;
    }
    // Exactly one normal component. This is what refuses `.`, `..`, and any
    // root/prefix form the OS would resolve somewhere other than where it reads.
    let mut components = Path::new(segment).components();
    matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none()
}

/// Resolve `segment` to the regular file it names inside `dir`, or say why not.
///
/// The SECOND gate: the canonical resolved path must still be inside the
/// canonical recordings directory. Canonicalising resolves symlinks, so this is
/// what refuses a link planted inside the directory that points out of it — the
/// case a name check cannot see.
///
/// A directory is refused rather than removed: a DELETE on this surface takes one
/// segment file and must never recurse.
async fn resolve_segment(dir: &Path, segment: &str) -> Result<PathBuf, SegmentReject> {
    if !is_plain_segment_name(segment) {
        return Err(SegmentReject::Invalid);
    }
    let base = tokio::fs::canonicalize(dir)
        .await
        .map_err(|_| SegmentReject::Absent)?;
    let resolved = tokio::fs::canonicalize(base.join(segment))
        .await
        .map_err(|_| SegmentReject::Absent)?;
    if !resolved.starts_with(&base) {
        return Err(SegmentReject::Invalid);
    }
    match tokio::fs::metadata(&resolved).await {
        Ok(meta) if meta.is_file() => Ok(resolved),
        _ => Err(SegmentReject::Absent),
    }
}

/// `DELETE .../recording/<segment>` → `{filename, deleted}`.
///
/// `404` `E_PROFILE_MISMATCH` off a drone-profile node, `400`
/// `E_INVALID_SEGMENT` for a name that is not one plain entry inside the
/// recordings directory (a traversal attempt lands here), `404`
/// `E_SEGMENT_NOT_FOUND` when it names nothing removable.
pub async fn delete_recording_segment(
    State(state): State<AppState>,
    AxumPath(segment): AxumPath<String>,
) -> Response {
    if !is_ground_station(&state) {
        return profile_mismatch();
    }
    let resolved = match resolve_segment(&recordings_dir(), &segment).await {
        Ok(path) => path,
        Err(SegmentReject::Invalid) => {
            tracing::warn!(segment = %segment, "recording delete refused: not a plain segment name");
            return error_body(
                StatusCode::BAD_REQUEST,
                "E_INVALID_SEGMENT",
                "segment must name one file inside the recordings directory",
            );
        }
        Err(SegmentReject::Absent) => {
            return error_body(
                StatusCode::NOT_FOUND,
                "E_SEGMENT_NOT_FOUND",
                "no such recording segment",
            )
        }
    };
    match tokio::fs::remove_file(&resolved).await {
        Ok(()) => json_ok(json!({"filename": segment, "deleted": true})),
        Err(err) => {
            tracing::warn!(segment = %segment, error = %err, "recording delete failed");
            error_body(
                StatusCode::INTERNAL_SERVER_ERROR,
                "E_SEGMENT_DELETE_FAILED",
                "the recording segment could not be removed",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Read a response body as JSON.
    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Read a response body as bytes.
    async fn body_bytes(resp: Response) -> Vec<u8> {
        axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec()
    }

    #[tokio::test]
    async fn profile_mismatch_body_is_the_fastapi_404_shape() {
        let resp = profile_mismatch();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body_json(resp).await,
            json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})
        );
    }

    // --- the listing --------------------------------------------------------

    #[tokio::test]
    async fn list_of_an_absent_dir_is_empty() {
        // A fresh ground station that has never recorded has no recordings dir;
        // the list is empty (never an error), matching the Python `is_dir()` guard.
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("never-created");
        assert_eq!(list_recordings(&absent).await, Vec::<Value>::new());
    }

    #[tokio::test]
    async fn list_of_an_empty_dir_is_empty() {
        // An existing-but-empty recordings dir also yields the empty list.
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(list_recordings(dir.path()).await, Vec::<Value>::new());
    }

    #[tokio::test]
    async fn list_enumerates_mp4_files_with_the_three_fields() {
        // Two .mp4 files land in the list, each with filename + size_bytes + mtime;
        // a non-.mp4 file and a subdirectory are both excluded.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.mp4"), b"aaaa").unwrap();
        fs::write(dir.path().join("b.mp4"), b"bbbbbbbb").unwrap();
        fs::write(dir.path().join("notes.txt"), b"x").unwrap();
        fs::create_dir(dir.path().join("subdir.mp4")).unwrap();

        let items = list_recordings(dir.path()).await;
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

    #[tokio::test]
    async fn list_reports_size_bytes_from_the_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("clip.mp4"), b"0123456789").unwrap();
        let items = list_recordings(dir.path()).await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["filename"], json!("clip.mp4"));
        assert_eq!(items[0]["size_bytes"], json!(10));
    }

    #[tokio::test]
    async fn list_is_newest_first_by_mtime() {
        // Two files with explicit, distinct mtimes must come back newest-first.
        use std::time::{Duration as StdDuration, SystemTime};
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.mp4");
        let new = dir.path().join("new.mp4");
        fs::write(&old, b"o").unwrap();
        fs::write(&new, b"n").unwrap();

        let base = SystemTime::now();
        filetime_set(&old, base - StdDuration::from_secs(100));
        filetime_set(&new, base);

        let items = list_recordings(dir.path()).await;
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["filename"], json!("new.mp4"));
        assert_eq!(items[1]["filename"], json!("old.mp4"));
    }

    #[tokio::test]
    async fn mp4_suffix_match_is_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("upper.MP4"), b"x").unwrap();
        fs::write(dir.path().join("mixed.Mp4"), b"x").unwrap();
        assert_eq!(list_recordings(dir.path()).await.len(), 2);
    }

    #[tokio::test]
    async fn a_symlink_named_mp4_is_not_a_recording_and_does_not_break_the_scan() {
        // The extension matches on both links, but neither is a file this
        // directory holds: `DirEntry::metadata` is `symlink_metadata` on Unix (in
        // `tokio::fs` exactly as in `std::fs`), so a link reads as a link and is
        // filtered, and the real recording beside it is still listed. A dangling
        // link is the case that could have thrown instead of skipping.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("real.mp4"), b"rrrr").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path().join("gone"), dir.path().join("dangling.mp4"))
                .unwrap();
            std::os::unix::fs::symlink(dir.path().join("real.mp4"), dir.path().join("alias.mp4"))
                .unwrap();
        }

        let items = list_recordings(dir.path()).await;
        let names: Vec<&str> = items
            .iter()
            .filter_map(|i| i.get("filename").and_then(Value::as_str))
            .collect();
        assert_eq!(names, vec!["real.mp4"]);
    }

    /// Set a file's mtime directly so the newest-first ordering test is
    /// deterministic, without depending on filesystem write-order timing.
    fn filetime_set(path: &Path, when: std::time::SystemTime) {
        // `File::set_modified` sets the mtime directly (no extra dependency).
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(when).unwrap();
    }

    // --- the stream-path + clip parameter gates -----------------------------

    #[test]
    fn stream_path_allow_list_refuses_separators_and_parent_hops() {
        assert!(is_valid_stream_path("main"));
        assert!(is_valid_stream_path("cam-1_sub.h264"));
        // Empty, traversal, separators, and anything that could break a request
        // line are all refused.
        assert!(!is_valid_stream_path(""));
        assert!(!is_valid_stream_path(".."));
        assert!(!is_valid_stream_path("../../etc/ados"));
        assert!(!is_valid_stream_path("a/b"));
        assert!(!is_valid_stream_path("a b"));
        assert!(!is_valid_stream_path("main\r\nX-Evil: 1"));
        assert!(!is_valid_stream_path(&"m".repeat(MAX_STREAM_PATH + 1)));
    }

    #[test]
    fn an_unparseable_start_is_refused_before_the_proxy() {
        let query = ClipQuery {
            path: Some("main".to_string()),
            start: Some("yesterday afternoon".to_string()),
            duration: Some("10".to_string()),
        };
        let reject = validated_clip(&query).expect_err("an unparseable start is refused");
        assert_eq!(reject.code, "E_INVALID_START");
    }

    #[test]
    fn a_non_positive_or_absurd_duration_is_refused() {
        for bad in ["0", "-5", "36000", "nan", "inf", "soon", ""] {
            let query = ClipQuery {
                path: Some("main".to_string()),
                start: Some("2026-09-03T12:00:00Z".to_string()),
                duration: Some(bad.to_string()),
            };
            let reject =
                validated_clip(&query).expect_err(&format!("duration {bad:?} must be refused"));
            assert_eq!(
                reject.code, "E_INVALID_DURATION",
                "duration {bad:?} must be refused as a duration"
            );
        }
    }

    #[test]
    fn a_stream_path_outside_the_allow_list_is_refused_on_the_clip_route_too() {
        let query = ClipQuery {
            path: Some("../../etc/ados".to_string()),
            start: Some("2026-09-03T12:00:00Z".to_string()),
            duration: Some("10".to_string()),
        };
        let reject = validated_clip(&query).expect_err("a traversal path is refused");
        assert_eq!(reject.code, "E_INVALID_PATH");
    }

    #[test]
    fn a_valid_clip_query_forwards_its_three_parameters() {
        let query = ClipQuery {
            path: Some("main".to_string()),
            start: Some("2026-09-03T12:00:00Z".to_string()),
            duration: Some("30.5".to_string()),
        };
        let (path, start, duration) = validated_clip(&query).expect("valid");
        assert_eq!(path, "main");
        assert_eq!(start, "2026-09-03T12:00:00Z");
        assert!((duration - 30.5).abs() < f64::EPSILON);
    }

    #[test]
    fn a_timestamp_offset_is_percent_encoded_not_turned_into_a_space() {
        // A raw `+` in a query decodes as a space, so a non-UTC offset must be
        // encoded or mediamtx receives a timestamp it cannot parse.
        assert_eq!(
            encode_query_value("2026-09-03T12:00:00+05:30"),
            "2026-09-03T12%3A00%3A00%2B05%3A30"
        );
    }

    // --- the playback proxy -------------------------------------------------

    /// Serve exactly one canned HTTP response on an ephemeral loopback port,
    /// standing in for the mediamtx playback server.
    async fn serve_once(response: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                // Drain the request first. Writing before reading would leave the
                // client's request bytes arriving at a socket this task then
                // closes, which the OS answers with an RST that discards the
                // response we just wrote.
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });
        port
    }

    #[tokio::test]
    async fn the_playback_proxy_relays_the_upstream_status_and_body() {
        let port = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 31\r\n\r\n\
             [{\"start\":\"x\",\"duration\":60.0}]",
        )
        .await;
        let upstream = playback_get(port, "/list?path=main")
            .await
            .expect("the loopback playback server answered");
        let relayed = crate::proxy::relay_response(upstream);
        assert_eq!(relayed.status(), StatusCode::OK);
        assert_eq!(
            body_json(relayed).await,
            json!([{"start": "x", "duration": 60.0}])
        );
    }

    #[tokio::test]
    async fn the_playback_proxy_relays_a_binary_clip_body() {
        // An fMP4 clip is not JSON; the relay must pass the bytes and the content
        // type through untouched so a browser `<video>` can play them.
        let port = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nContent-Length: 4\r\n\r\nftyp",
        )
        .await;
        let upstream = playback_get(port, "/get?path=main&start=x&duration=1")
            .await
            .expect("the loopback playback server answered");
        let relayed = crate::proxy::relay_response(upstream);
        assert_eq!(
            relayed
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("video/mp4")
        );
        assert_eq!(body_bytes(relayed).await, b"ftyp".to_vec());
    }

    #[tokio::test]
    async fn a_closed_playback_port_is_a_503_not_a_hang() {
        // Bind an ephemeral port and drop the listener so the connect is refused.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        assert!(playback_get(port, "/list?path=main").await.is_none());
        let resp = playback_unavailable();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body_json(resp).await["detail"]["error"]["code"],
            json!("E_PLAYBACK_UNAVAILABLE")
        );
    }

    // --- the DELETE traversal gate -----------------------------------------

    #[test]
    fn a_segment_name_with_a_separator_or_parent_hop_is_not_a_plain_name() {
        assert!(is_plain_segment_name("2026-09-03_12-00-00-000000.mp4"));
        assert!(!is_plain_segment_name(""));
        assert!(!is_plain_segment_name("."));
        assert!(!is_plain_segment_name(".."));
        assert!(!is_plain_segment_name("../config.yaml"));
        assert!(!is_plain_segment_name("../../etc/ados/config.yaml"));
        assert!(!is_plain_segment_name("main/seg.mp4"));
        assert!(!is_plain_segment_name("/etc/ados/config.yaml"));
        assert!(!is_plain_segment_name("a\\b"));
        assert!(!is_plain_segment_name("seg..mp4"));
        assert!(!is_plain_segment_name(&"x".repeat(MAX_SEGMENT_NAME + 1)));
    }

    #[tokio::test]
    async fn a_traversal_attempt_is_refused_and_leaves_the_target_alone() {
        // The hazard: the DELETE takes its target from the URL and axum
        // percent-decodes it, so `%2e%2e%2f` arrives as `../`. The file outside
        // the recordings directory must survive.
        let root = tempfile::tempdir().unwrap();
        let recordings = root.path().join("recordings");
        fs::create_dir(&recordings).unwrap();
        let outsider = root.path().join("config.yaml");
        fs::write(&outsider, b"agent:\n  name: x\n").unwrap();

        for attempt in ["../config.yaml", "../../etc/ados/config.yaml", ".."] {
            assert_eq!(
                resolve_segment(&recordings, attempt).await,
                Err(SegmentReject::Invalid),
                "{attempt} must be refused as a name, never resolved"
            );
        }
        assert!(outsider.exists(), "the traversal target must be untouched");
    }

    #[tokio::test]
    async fn a_symlink_pointing_out_of_the_recordings_dir_is_refused() {
        // The case a name check cannot see: a plain, separator-free name whose
        // resolved path is outside the directory.
        let root = tempfile::tempdir().unwrap();
        let recordings = root.path().join("recordings");
        fs::create_dir(&recordings).unwrap();
        let outsider = root.path().join("config.yaml");
        fs::write(&outsider, b"agent:\n  name: x\n").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outsider, recordings.join("escape.mp4")).unwrap();
            assert_eq!(
                resolve_segment(&recordings, "escape.mp4").await,
                Err(SegmentReject::Invalid),
                "a link out of the recordings dir must be refused"
            );
            assert!(outsider.exists(), "the link target must be untouched");
        }
    }

    #[tokio::test]
    async fn a_real_segment_resolves_and_a_directory_does_not() {
        let recordings = tempfile::tempdir().unwrap();
        fs::write(recordings.path().join("seg.mp4"), b"data").unwrap();
        fs::create_dir(recordings.path().join("main")).unwrap();

        let resolved = resolve_segment(recordings.path(), "seg.mp4")
            .await
            .expect("a real segment resolves");
        assert!(resolved.ends_with("seg.mp4"));
        // A directory is not a segment: a DELETE here takes one file, never a tree.
        assert_eq!(
            resolve_segment(recordings.path(), "main").await,
            Err(SegmentReject::Absent)
        );
        assert_eq!(
            resolve_segment(recordings.path(), "nope.mp4").await,
            Err(SegmentReject::Absent)
        );
    }
}
