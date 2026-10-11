//! The recording segments and clip routes, served off the mediamtx playback server.

use super::*;

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
pub(super) struct ClipReject {
    pub(super) code: &'static str,
    pub(super) message: &'static str,
}

/// Validate and bound the `/clip` query, returning the triple to forward or why
/// it is refused. Every parameter is checked: an unparseable `start` and a
/// non-positive or absurd `duration` are refused here rather than forwarded for
/// mediamtx to answer about.
pub(super) fn validated_clip(query: &ClipQuery) -> Result<(String, String, f64), ClipReject> {
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
