//! Video block: the multi-process branch (no in-process pipeline).

use super::super::*;
use crate::mediamtx_probe::http_get_local;
use ados_protocol::paths::run_dir;
use serde_json::json;
use serde_json::Map;
use serde_json::Value;
use std::path::Path;

/// mediamtx default ports.
pub(crate) const MEDIAMTX_API_PORT: u16 = 9997;
pub(crate) const MEDIAMTX_WEBRTC_PORT: u16 = 8889;

/// Build the consolidated-status `video` block for the multi-process case.
///
/// There is no in-process pipeline on this daemon, so the block branches
/// on the resolved profile (NOT on pipeline presence):
///
/// - **drone** — the local mediamtx serves its own camera regardless of the WFB
///   downlink. Probe readiness (`/v3/paths/list`, then the WHEP fallback); when
///   ready, report `running` with the WHEP URL, else the default `not_initialized`.
/// - **ground-station** — mediamtx serves WHEP whether or not frames arrive, so a
///   reachable endpoint is not proof of a live downlink. Gate on the WFB receive
///   link actually delivering (`_gs_video_delivering`): when delivering + WHEP
///   ready → `running`; delivering but WHEP not yet serving → `connecting`; not
///   delivering → `stopped`.
///
/// The recording block is always empty (no in-process recorder on this daemon).
///
/// This resolves the live mediamtx readiness once (the real loopback probes) and
/// hands it to the pure [`build_video_block_with`] core, so the block-shape logic
/// is testable with an injected readiness and no live port.
pub(crate) async fn build_video_block(
    resolved_profile: &str,
    wfb_status: &Option<Map<String, Value>>,
) -> Value {
    build_video_block_with(
        resolved_profile,
        wfb_status,
        resolve_mediamtx_ready().await,
        read_video_streams(),
        read_pipeline_outcome(),
    )
}

/// The pipeline's own answer, read out of the `camera-state.json` sidecar that
/// ados-video re-stamps at every lifecycle transition.
///
/// `fresh` is the load-bearing field: it separates "the writer is alive and
/// simply did not name a state" (a legacy ados-video, which still deserves a
/// playable endpoint) from "nobody has written this file recently" (the writer
/// died, and the `streaming` it left behind must not keep a dead node dialable).
#[derive(Debug, Default, Clone)]
pub(crate) struct PipelineSidecar {
    /// The sidecar was present, parseable AND within [`CAMERA_STATE_FRESH_S`].
    pub(crate) fresh: bool,
    /// `pipeline_state`. Absent under `fresh` means a legacy writer.
    pub(crate) state: Option<String>,
    pub(crate) reason: Option<String>,
    pub(crate) encoder: Option<String>,
    /// `encoder_hw`: whether that encoder is hardware-backed. Absent on a
    /// writer that predates the field.
    pub(crate) encoder_hw: Option<bool>,
    /// `encoder_forced_software`: the hardware encoder failed and the next
    /// start was pinned to the software encoder. Absent on a writer that
    /// predates the field.
    pub(crate) encoder_forced_software: Option<bool>,
}

/// Read the pipeline sidecar the way every other sidecar block in this file
/// reads its own: resolved under [`run_dir`], staleness-gated on
/// `updated_at_unix`.
///
/// This exists because mediamtx readiness is NOT evidence that video is
/// flowing: the WHEP fallback probe accepts 200/204/405 on `:8889/main/whep`,
/// which mediamtx serves with ZERO publishers, so a node whose encoder never
/// started still reported `state: "running"` with a dialable `whep_url`. The
/// freshness gate closes the same hole from the other side: when ados-video
/// dies, mediamtx stays bound (a separate unit) and the sidecar left behind
/// still says `streaming`, so an unread clock kept a dead node dialable
/// forever.
pub(crate) fn read_pipeline_outcome() -> PipelineSidecar {
    read_pipeline_outcome_in(&run_dir(), now_unix_secs())
}

/// The path-injectable core of [`read_pipeline_outcome`], so a test drives it
/// with a tempdir instead of the process-global `ADOS_RUN_DIR`.
pub(crate) fn read_pipeline_outcome_in(run_dir: &Path, now: f64) -> PipelineSidecar {
    let Some(doc) = crate::sidecar::read_object(&run_dir.join("camera-state.json")) else {
        return PipelineSidecar::default();
    };
    if !sidecar_fresh(&doc, now, CAMERA_STATE_FRESH_S) {
        return PipelineSidecar::default();
    }
    PipelineSidecar {
        fresh: true,
        state: doc
            .get("pipeline_state")
            .and_then(Value::as_str)
            .map(str::to_string),
        reason: doc
            .get("pipeline_reason")
            .and_then(Value::as_str)
            .map(str::to_string),
        encoder: doc
            .get("encoder")
            .and_then(Value::as_str)
            .map(str::to_string),
        encoder_hw: doc.get("encoder_hw").and_then(Value::as_bool),
        encoder_forced_software: doc.get("encoder_forced_software").and_then(Value::as_bool),
    }
}

/// Read the video-streams sidecar (`/run/ados/video-streams.json`, written by
/// ados-video on pipeline start) and resolve each leg to the agent's own
/// same-origin WHEP + HLS proxy paths, so the status surface advertises every leg
/// stream switcher. Best-effort: any read/parse failure yields an empty list.
pub(crate) fn read_video_streams() -> Vec<Value> {
    let Ok(body) = std::fs::read("/run/ados/video-streams.json") else {
        return Vec::new();
    };
    let Ok(doc) = serde_json::from_slice::<Value>(&body) else {
        return Vec::new();
    };
    let Some(streams) = doc.get("streams").and_then(|s| s.as_array()) else {
        return Vec::new();
    };
    streams
        .iter()
        .filter_map(|leg| {
            let id = leg.get("id")?.as_str()?;
            Some(json!({
                "id": id,
                "role": leg.get("role").and_then(|r| r.as_str()).unwrap_or(""),
                "codec": leg.get("codec").and_then(|c| c.as_str()).unwrap_or(""),
                "whepUrl": format!("/whep?camera={id}"),
                "hls": format!("/hls/{id}/index.m3u8"),
                // Per-leg liveness (null when the agent has not sampled it yet).
                "live": leg.get("live").cloned().unwrap_or(Value::Null),
            }))
        })
        .collect()
}

/// Resolve whether the local mediamtx has a ready stream: the management-API
/// probe (`/v3/paths/list`) with the WHEP-liveness fallback for the credentialed
/// ground-station mediamtx. Split out from the block builder so the builder is a
/// pure function of an injected readiness (no live TCP port in unit tests).
pub(crate) async fn resolve_mediamtx_ready() -> bool {
    let mut mtx = probe_mediamtx().await;
    if mtx.as_ref().map(|m| !mtx_ready(m)).unwrap_or(true) {
        mtx = probe_mediamtx_via_whep().await.or(mtx);
    }
    mtx.as_ref().map(mtx_ready).unwrap_or(false)
}

/// The injectable core of [`build_video_block`]. Pure over `mediamtx_ready`
/// (the resolved probe outcome) and `pipeline` (the sidecar verdict), so tests
/// exercise every branch deterministically without a live loopback port.
///
/// THE RULE, after a node reported `state: "running"` with a dialable
/// `whep_url` while its pipeline sat in `Error`: mediamtx readiness alone is
/// NOT evidence that video exists. The WHEP fallback probe accepts 200/204/405
/// on an endpoint mediamtx serves with zero publishers, so it proves only that
/// mediamtx is bound. The pipeline's own verdict outranks it, and a state that
/// is not `streaming` NEVER gets a `whep_url`.
///
/// The one exception is a FRESH sidecar carrying no `pipeline_state` at all: a
/// legacy ados-video that is alive and writing. Answering `connecting` there
/// would cost a mid-upgrade fleet its cloud video AND its auto-retry, both of
/// which the cockpit gates on `videoState === "running"`. That branch keeps the
/// previous answer and publishes `whep_url_basis`, so a consumer can see the
/// endpoint rests on mediamtx readiness alone instead of having to guess.
pub(crate) fn build_video_block_with(
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
    } = pipeline;

    // The encoder identity 0.99.359 dropped: without it the GCS cannot tell a
    // software fallback from a hardware encoder, or either from silence. The
    // label is the on-wire value ados-video derives from the command it built
    // (`rpicam-vid`, `ffmpeg-h264_v4l2m2m`, `gstreamer-x264enc`, and so on);
    // `encoder_hw` answers the hardware question as a field, so no consumer has
    // to re-encode a list of codec element names to sniff it out of the string.
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
        // Advertise the per-leg streams (only when a live pipeline wrote the
        // sidecar) so the GCS switcher can address each leg's same-origin path.
        if !streams.is_empty() {
            block["streams"] = Value::Array(streams.clone());
        }
        block
    };

    let mut block = if pipeline_state.as_deref() == Some("error") {
        // A pipeline that FAILED outranks every readiness probe, on either profile.
        let mut block = plain("error");
        block["reason"] = match pipeline_reason.as_deref() {
            Some(r) => json!(r),
            None => json!("the video pipeline is in an error state"),
        };
        block
    } else if resolved_profile == "drone" {
        // On the air side the pipeline IS the publisher, so its verdict is the
        // question. `streaming` plus a ready mediamtx is the only "running".
        match (pipeline_state.as_deref(), mediamtx_ready) {
            (Some("streaming"), true) => running(),
            // Publishing but mediamtx has not caught up yet.
            (Some("streaming"), false) => plain("connecting"),
            // mediamtx is bound and the pipeline has not said it is publishing:
            // exactly the case the old code called "running".
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
            // Everything else — no sidecar at all, a stale one (the writer
            // died), or a current writer explicitly saying `unknown` — has not
            // earned a playable endpoint.
            _ => plain("not_initialized"),
        }
    } else if gs_video_delivering(wfb_status.as_ref()) {
        // Ground-station path: gate on the receive link actually delivering
        // video. There is no local pipeline here — the publisher is the drone.
        if mediamtx_ready {
            running()
        } else {
            // Link delivering frames but WHEP not yet serving — still coming up.
            plain("connecting")
        }
    } else {
        // No live downlink: stopped with no playable endpoint.
        plain("stopped")
    };

    // Report what the pipeline said on every branch where it answered, so a
    // consumer can tell "the agent says it is streaming" from "the agent cannot
    // say". A fresh sidecar with no key is a legacy writer: `unknown`.
    if fresh {
        block["pipeline_state"] = json!(pipeline_state.as_deref().unwrap_or("unknown"));
    }
    block
}

/// True only when the ground-station WFB link is actually delivering video: the receive
/// state must be `active`/`connected` AND a positive valid-decode rate or packet count
/// confirms frames are flowing now. A reachable WHEP endpoint is not proof (mediamtx
/// serves WHEP regardless of inbound frames).
pub(crate) fn gs_video_delivering(wfb_status: Option<&Map<String, Value>>) -> bool {
    let Some(status) = wfb_status else {
        return false;
    };
    let state = status.get("state").and_then(Value::as_str);
    if !matches!(state, Some("active") | Some("connected")) {
        return false;
    }
    for key in ["valid_rx_packets_per_s", "packets_received"] {
        if let Some(v) = status.get(key).and_then(Value::as_f64) {
            if v > 0.0 {
                return true;
            }
        }
    }
    false
}

/// True when a mediamtx probe result reports a ready stream (`ready == true`).
pub(crate) fn mtx_ready(mtx: &Value) -> bool {
    mtx.get("ready").and_then(Value::as_bool).unwrap_or(false)
}

/// Probe mediamtx's management API (`/v3/paths/list`) for an active stream.
///
/// Returns a small result dict (`running`, `stream_name`, `ready`, `tracks`,
/// `readers`, `webrtc_port`) for the `main` path, or `None` when mediamtx is
/// unreachable / returns non-200 / lists no `main` path.
pub(crate) async fn probe_mediamtx() -> Option<Value> {
    let data = crate::mediamtx_probe::read_paths_list().await?;
    // The primary `main` path BY NAME, with no fallback to whichever path is
    // listed first: with several named paths (main / eo_wide / ir) another leg's
    // state says nothing about the stream the advertised URLs address.
    let path = crate::mediamtx_probe::main_path(&data)?;
    Some(json!({
        "running": true,
        "stream_name": "main",
        "ready": crate::mediamtx_probe::path_ready(path),
        "tracks": path.get("tracks").cloned().unwrap_or_else(|| json!([])),
        "readers": path.get("readers").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0),
        "webrtc_port": MEDIAMTX_WEBRTC_PORT,
    }))
}

/// Liveness probe via the public WHEP endpoint, for the ground-station mediamtx
/// (which puts auth on the management API). A GET on the POST-only WHEP path
/// returns 200/204/405 when bound — the "endpoint exists and mediamtx is up"
/// signal, no credentials needed. Returns the same result shape with `ready: true`
/// (the fanout exposes no separate readiness here).
pub(crate) async fn probe_mediamtx_via_whep() -> Option<Value> {
    let url = format!("http://127.0.0.1:{MEDIAMTX_WEBRTC_PORT}/main/whep");
    let (status, _body) = http_get_local(&url).await.ok()?;
    if matches!(status, 200 | 204 | 405) {
        return Some(json!({
            "running": true,
            "stream_name": "main",
            "ready": true,
            "tracks": [],
            "readers": 0,
            "webrtc_port": MEDIAMTX_WEBRTC_PORT,
        }));
    }
    None
}

/// The cumulative `bytesReceived` counter on the mediamtx `main` path, read from
/// the management API `/v3/paths/get/main`. This is the canonical
/// video-into-mediamtx signal (the drone's encoder → mediamtx ingest, or the
/// ground station's fan-out → mediamtx-gs ingest), and the reliable delta the
/// video-diagnostics harness samples. `None` when the API is unreachable, returns
/// non-200 (the ground-station mediamtx puts auth on the management API, so this
/// can legitimately be unavailable there and the caller falls back to the WHEP
/// liveness probe), or the body carries no numeric `bytesReceived`.
pub(crate) async fn mediamtx_main_bytes_received() -> Option<i64> {
    let url = format!("http://127.0.0.1:{MEDIAMTX_API_PORT}/v3/paths/get/main");
    let (status, body) = http_get_local(&url).await.ok()?;
    if status != 200 {
        return None;
    }
    let data: Value = serde_json::from_slice(&body).ok()?;
    data.get("bytesReceived")
        .and_then(Value::as_i64)
        .or_else(|| {
            data.get("bytesReceived")
                .and_then(Value::as_f64)
                .map(|f| f as i64)
        })
}

/// True when the mediamtx WHEP endpoint (`:8889/main/whep`) is bound and serving.
/// A GET on the POST-only WHEP path returns 200/204/405 when mediamtx is up —
/// the credential-free "endpoint exists and mediamtx is serving" signal the
/// video-diagnostics harness uses for the served-WHEP hop (and the ground-station
/// ingest hop when the management API is auth-gated).
pub(crate) async fn mediamtx_whep_serving() -> bool {
    probe_mediamtx_via_whep().await.is_some()
}
