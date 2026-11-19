//! The video verdict every status surface reports: the LAN consolidated status
//! (`/api/status/full`) and the cloud heartbeat read the same answer.
//!
//! The verdict is a pure function ([`build_video_block_with`]) of the resolved
//! profile, the WFB receive status, the local mediamtx readiness
//! ([`resolve_mediamtx_ready`]) and the pipeline's own word, read out of the
//! `camera-state.json` sidecar ([`read_pipeline_outcome`]).
//!
//! The primary stream is always published at the fixed `main` path, and that is
//! the path `/whep` and `/hls/main/index.m3u8` address. Readiness is therefore
//! judged on `main` alone: a secondary leg (`eo_wide`, `ir`, a plugin camera)
//! being ready says nothing about whether the advertised URL plays. A path is
//! ready only when mediamtx reports `ready: true` AND a non-null `source` (a
//! publisher is attached); an absent `main` is not ready.

use std::path::Path;

use ados_protocol::paths::{run_dir, CAMERA_STATE_MAX_AGE_MS};
use serde_json::{json, Map, Value};

use crate::mediamtx::net::http_get_status;
use crate::mediamtx::{DEFAULT_API_PORT, DEFAULT_WEBRTC_PORT};

/// How long a `camera-state.json` stays evidence that its writer is alive.
pub const CAMERA_STATE_FRESH_S: f64 = CAMERA_STATE_MAX_AGE_MS as f64 / 1000.0;

/// How far ahead of `now` the sidecar's `updated_at_unix` may sit and still
/// count. Writer and reader share one clock, so anything beyond rounding is a
/// clock step and the age is unprovable.
const SIDECAR_FUTURE_SKEW_S: f64 = 2.0;

/// The management API's `/v3/paths/list` body, or `None` when mediamtx is
/// unreachable, answers non-200 (the ground-station mediamtx puts credentials on
/// this API), or returns a body that is not JSON.
pub async fn read_paths_list() -> Option<Value> {
    let (status, body) = http_get_status(DEFAULT_API_PORT, "/v3/paths/list").await?;
    if status != 200 {
        return None;
    }
    serde_json::from_slice(&body).ok()
}

/// The `main` path object out of a `/v3/paths/list` body, looked up by name.
/// There is deliberately no fallback to another path: `None` when `main` is not
/// listed, whatever else is.
pub fn main_path(list: &Value) -> Option<&Map<String, Value>> {
    list.get("items")?
        .as_array()?
        .iter()
        .filter_map(Value::as_object)
        .find(|p| p.get("name").and_then(Value::as_str) == Some("main"))
}

/// True when a mediamtx path object reports a live publisher: `ready: true` and
/// a non-null `source`.
pub fn path_ready(path: &Map<String, Value>) -> bool {
    path.get("ready").and_then(Value::as_bool).unwrap_or(false)
        && path.get("source").is_some_and(|s| !s.is_null())
}

/// True when a mediamtx probe result reports a ready stream (`ready == true`).
pub fn mtx_ready(mtx: &Value) -> bool {
    mtx.get("ready").and_then(Value::as_bool).unwrap_or(false)
}

/// Probe mediamtx's management API (`/v3/paths/list`) for the `main` stream.
///
/// Returns a small result dict (`running`, `stream_name`, `ready`, `tracks`,
/// `readers`, `webrtc_port`), or `None` when mediamtx is unreachable / returns
/// non-200 / lists no `main` path.
pub async fn probe_mediamtx() -> Option<Value> {
    let data = read_paths_list().await?;
    let path = main_path(&data)?;
    Some(json!({
        "running": true,
        "stream_name": "main",
        "ready": path_ready(path),
        "tracks": path.get("tracks").cloned().unwrap_or_else(|| json!([])),
        "readers": path.get("readers").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0),
        "webrtc_port": DEFAULT_WEBRTC_PORT,
    }))
}

/// Liveness probe via the public WHEP endpoint, for the ground-station mediamtx
/// (which puts auth on the management API). A GET on the POST-only WHEP path
/// returns 200/204/405 when bound: the "endpoint exists and mediamtx is up"
/// signal, no credentials needed. Returns the same result shape with
/// `ready: true` (the fanout exposes no separate readiness here).
pub async fn probe_mediamtx_via_whep() -> Option<Value> {
    let (status, _body) = http_get_status(DEFAULT_WEBRTC_PORT, "/main/whep").await?;
    matches!(status, 200 | 204 | 405).then(|| {
        json!({
            "running": true,
            "stream_name": "main",
            "ready": true,
            "tracks": [],
            "readers": 0,
            "webrtc_port": DEFAULT_WEBRTC_PORT,
        })
    })
}

/// Resolve whether the local mediamtx has a ready stream: the management-API
/// probe (`/v3/paths/list`) with the WHEP-liveness fallback for the credentialed
/// ground-station mediamtx. Split out from the block builder so the builder is a
/// pure function of an injected readiness (no live TCP port in unit tests).
pub async fn resolve_mediamtx_ready() -> bool {
    let mut mtx = probe_mediamtx().await;
    if mtx.as_ref().map(|m| !mtx_ready(m)).unwrap_or(true) {
        mtx = probe_mediamtx_via_whep().await.or(mtx);
    }
    mtx.as_ref().map(mtx_ready).unwrap_or(false)
}

/// True when the mediamtx WHEP endpoint (`:8889/main/whep`) is bound and
/// serving: the credential-free signal for the served-WHEP hop.
pub async fn mediamtx_whep_serving() -> bool {
    probe_mediamtx_via_whep().await.is_some()
}

/// The cumulative `bytesReceived` counter on the mediamtx `main` path, from the
/// management API `/v3/paths/get/main`: the video-into-mediamtx signal. `None`
/// when the API is unreachable, answers non-200 (the ground-station mediamtx
/// puts auth on it) or the body carries no numeric `bytesReceived`.
pub async fn mediamtx_main_bytes_received() -> Option<i64> {
    let (status, body) = http_get_status(DEFAULT_API_PORT, "/v3/paths/get/main").await?;
    if status != 200 {
        return None;
    }
    let data: Value = serde_json::from_slice(&body).ok()?;
    let v = data.get("bytesReceived")?;
    v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))
}

/// The pipeline's own answer, read out of the `camera-state.json` sidecar that
/// ados-video re-stamps at every lifecycle transition.
///
/// `fresh` is the load-bearing field: it separates "the writer is alive and
/// simply did not name a state" (a legacy ados-video, which still deserves a
/// playable endpoint) from "nobody has written this file recently" (the writer
/// died, and the `streaming` it left behind must not keep a dead node dialable).
#[derive(Debug, Default, Clone)]
pub struct PipelineSidecar {
    /// The sidecar was present, parseable AND within [`CAMERA_STATE_FRESH_S`].
    pub fresh: bool,
    /// `pipeline_state`. Absent under `fresh` means a legacy writer.
    pub state: Option<String>,
    pub reason: Option<String>,
    pub encoder: Option<String>,
    /// `encoder_hw`: whether that encoder is hardware-backed. Absent on a
    /// writer that predates the field.
    pub encoder_hw: Option<bool>,
    /// `encoder_forced_software`: the hardware encoder failed and the next
    /// start was pinned to the software encoder. Absent on a writer that
    /// predates the field.
    pub encoder_forced_software: Option<bool>,
    /// `framebus`: per captured camera, the frame-bus feed counters. Absent
    /// when no encoder reads the frame bus.
    pub framebus: Option<Value>,
}

/// Read the pipeline sidecar under [`run_dir`], staleness-gated on
/// `updated_at_unix`.
///
/// mediamtx readiness is NOT evidence that video is flowing: the WHEP fallback
/// probe accepts 200/204/405 on `:8889/main/whep`, which mediamtx serves with
/// ZERO publishers. The freshness gate closes the same hole from the other
/// side: when ados-video dies, mediamtx stays bound (a separate unit) and the
/// sidecar left behind still says `streaming`.
pub fn read_pipeline_outcome() -> PipelineSidecar {
    read_pipeline_outcome_in(&run_dir(), now_unix_secs())
}

/// The path-injectable core of [`read_pipeline_outcome`].
pub fn read_pipeline_outcome_in(run_dir: &Path, now: f64) -> PipelineSidecar {
    let Some(doc) = std::fs::read_to_string(run_dir.join("camera-state.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    else {
        return PipelineSidecar::default();
    };
    let fresh = doc
        .get("updated_at_unix")
        .and_then(Value::as_f64)
        .is_some_and(|u| (-SIDECAR_FUTURE_SKEW_S..=CAMERA_STATE_FRESH_S).contains(&(now - u)));
    if !fresh {
        return PipelineSidecar::default();
    }
    let text = |k: &str| doc.get(k).and_then(Value::as_str).map(str::to_string);
    PipelineSidecar {
        fresh: true,
        state: text("pipeline_state"),
        reason: text("pipeline_reason"),
        encoder: text("encoder"),
        encoder_hw: doc.get("encoder_hw").and_then(Value::as_bool),
        encoder_forced_software: doc.get("encoder_forced_software").and_then(Value::as_bool),
        framebus: doc.get("framebus").filter(|v| v.is_array()).cloned(),
    }
}

fn now_unix_secs() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// The consolidated-status `video` block. Pure over `mediamtx_ready` (the
/// resolved probe outcome) and `pipeline` (the sidecar verdict), so tests
/// exercise every branch without a live loopback port.
///
/// - **drone**: the pipeline IS the publisher, so its verdict is the question;
///   `streaming` plus a ready mediamtx is the only `running`.
/// - **ground-station**: mediamtx serves WHEP whether or not frames arrive, so
///   the block gates on the WFB receive link actually delivering
///   ([`gs_video_delivering`]): delivering + WHEP ready → `running`; delivering
///   but WHEP not yet serving → `connecting`; not delivering → `stopped`.
///
/// A pipeline in `error` outranks every readiness probe on either profile, and
/// a state that is not `running` NEVER gets a `whep_url`. The one exception to
/// the pipeline's word is a FRESH sidecar carrying no `pipeline_state` at all (a
/// legacy ados-video, alive and writing): it keeps the playable answer and
/// publishes `whep_url_basis` so a consumer can see the endpoint rests on
/// mediamtx readiness alone.
pub fn build_video_block_with(
    resolved_profile: &str,
    wfb_status: &Option<Map<String, Value>>,
    mediamtx_ready: bool,
    streams: Vec<Value>,
    pipeline: PipelineSidecar,
) -> Value {
    let PipelineSidecar {
        fresh,
        state: pipeline_state,
        reason: pipeline_reason,
        encoder,
        encoder_hw,
        encoder_forced_software,
        framebus,
    } = pipeline;

    // The encoder identity: without it the GCS cannot tell a software fallback
    // from a hardware encoder, or either from silence.
    let with_encoder = |block: &mut Value| {
        if let Some(enc) = encoder.as_deref() {
            block["encoder"] = json!(enc);
        }
        if let Some(hw) = encoder_hw {
            block["encoder_hw"] = json!(hw);
        }
        if let Some(forced) = encoder_forced_software {
            block["encoder_forced_software"] = json!(forced);
        }
    };

    let plain = |state: &str| {
        let mut block = json!({
            "state": state,
            "whep_url": null,
            "recording": false,
            "recording_filename": null,
            "recording_started_at": null,
        });
        with_encoder(&mut block);
        block
    };

    let running = || {
        let mut block = json!({
            "state": "running",
            "whep_url": "/whep",
            "hls_url": "/hls/main/index.m3u8",
            "recording": false,
            "recording_filename": null,
            "recording_started_at": null,
        });
        with_encoder(&mut block);
        // The per-leg streams, so the GCS switcher can address each leg.
        if !streams.is_empty() {
            block["streams"] = Value::Array(streams.clone());
        }
        block
    };

    let mut block = if pipeline_state.as_deref() == Some("error") {
        let mut block = plain("error");
        block["reason"] = match pipeline_reason.as_deref() {
            Some(r) => json!(r),
            None => json!("the video pipeline is in an error state"),
        };
        block
    } else if resolved_profile == "drone" {
        match (pipeline_state.as_deref(), mediamtx_ready) {
            (Some("streaming"), true) => running(),
            // Publishing but mediamtx has not caught up yet.
            (Some("streaming"), false) => plain("connecting"),
            // mediamtx is bound and the pipeline has not said it is publishing.
            (Some("starting"), _) => plain("connecting"),
            (Some("stopped"), _) => plain("stopped"),
            // A legacy ados-video: no `pipeline_state` key, but the file is
            // fresh, which is positive evidence the writer is alive.
            (None, true) if fresh => {
                let mut block = running();
                block["whep_url_basis"] = json!("mediamtx_readiness_only");
                block
            }
            (None, false) if fresh => plain("connecting"),
            // No sidecar, a stale one, or a writer saying `unknown`: no
            // playable endpoint earned.
            _ => plain("not_initialized"),
        }
    } else if gs_video_delivering(wfb_status.as_ref()) {
        if mediamtx_ready {
            running()
        } else {
            plain("connecting")
        }
    } else {
        plain("stopped")
    };

    // Report what the pipeline said on every branch where it answered. A fresh
    // sidecar with no key is a legacy writer: `unknown`.
    if fresh {
        block["pipeline_state"] = json!(pipeline_state.as_deref().unwrap_or("unknown"));
    }
    if let Some(feeds) = framebus {
        block["framebus"] = feeds;
    }
    block
}

/// True only when the ground-station WFB link is actually delivering video: the
/// receive state must be `active`/`connected` AND a positive valid-decode rate
/// or packet count confirms frames are flowing now. A reachable WHEP endpoint is
/// not proof (mediamtx serves WHEP regardless of inbound frames).
pub fn gs_video_delivering(wfb_status: Option<&Map<String, Value>>) -> bool {
    let Some(status) = wfb_status else {
        return false;
    };
    let state = status.get("state").and_then(Value::as_str);
    if !matches!(state, Some("active") | Some("connected")) {
        return false;
    }
    ["valid_rx_packets_per_s", "packets_received"]
        .iter()
        .any(|k| status.get(*k).and_then(Value::as_f64).is_some_and(|v| v > 0.0))
}

#[cfg(test)]
mod tests;
