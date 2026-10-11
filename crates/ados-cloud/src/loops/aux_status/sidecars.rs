//! The camera, video and board sidecar readers the status producer samples.

use super::*;

/// Read the current camera state, staleness-gated through the shared
/// [`ados_video::camera_state::read_effective_state`] reader.
///
/// A lingering sidecar from a stopped pipeline must not keep advertising a
/// camera as ready, so an un-refreshed file reads as unknown rather than as its
/// last value, and a discovered camera over a failed pipeline reads as `error`.
pub(super) fn read_camera_state(path: &str, now: f64) -> Option<String> {
    read_effective_state(Path::new(path), now, CAMERA_STATE_LIVE_S)
        .map(|state| state.as_str().to_string())
}

/// Derive a one-word video state from the streams sidecar, staleness-gated.
///
/// The sidecar carries per-leg liveness, so this reports what is actually
/// happening rather than merely that a pipeline was configured: `streaming` when
/// at least one leg is receiving, `degraded` when every leg that reported
/// liveness is flat, and `idle` when legs exist but none reported. An absent or
/// stale sidecar reads as unknown, never as `stopped` — the producer does not
/// know the pipeline stopped, only that nobody said otherwise.
/// How many video legs the streams sidecar advertises, staleness-gated.
///
/// This is the only way a ground station learns that the drone it relays serves
/// more than one camera: the radio carries no IP, so the ground node cannot ask.
/// An absent or stale sidecar reads as `None` (unknown), never as one leg — a
/// pod shipping two concurrent legs must not be reported as single-camera
/// merely because nobody could read the file.
pub(super) fn read_video_leg_count(path: &str, now: f64) -> Option<u8> {
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let updated = doc.get("updated_at_unix").and_then(Value::as_f64)?;
    if updated <= 0.0 || now - updated > VIDEO_STREAMS_STALE_S {
        return None;
    }
    let streams = doc.get("streams").and_then(Value::as_array)?;
    u8::try_from(streams.len()).ok()
}

pub(super) fn read_video_state(path: &str, now: f64) -> Option<String> {
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let updated = doc.get("updated_at_unix").and_then(Value::as_f64)?;
    if updated <= 0.0 || now - updated > VIDEO_STREAMS_STALE_S {
        return None;
    }
    let streams = doc.get("streams").and_then(Value::as_array)?;
    if streams.is_empty() {
        return None;
    }
    let live: Vec<bool> = streams
        .iter()
        .filter_map(|s| s.get("live").and_then(Value::as_bool))
        .collect();
    Some(
        if live.iter().any(|l| *l) {
            "streaming"
        } else if live.is_empty() {
            "idle"
        } else {
            "degraded"
        }
        .to_string(),
    )
}

/// Read the board descriptors from the board sidecar: name, SoC, tier.
pub(super) fn read_board(path: &str) -> (Option<String>, Option<String>, Option<u8>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (None, None, None);
    };
    let Ok(doc) = serde_json::from_str::<Value>(&text) else {
        return (None, None, None);
    };
    let s = |k: &str| {
        doc.get(k)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    let tier = doc
        .get("tier")
        .and_then(Value::as_u64)
        .and_then(|t| u8::try_from(t).ok());
    (s("name"), s("soc"), tier)
}
