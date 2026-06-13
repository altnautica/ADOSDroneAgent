//! Video pipeline read routes: latency, the air-side pipeline snapshot, and the
//! encoder/radio config snapshot.
//!
//! Three read-only routes the GCS Video Link panel + the LCD page poll:
//!
//! - **`GET /api/video/latency`** — the most-recent SEI-probe glass-to-glass
//!   latency. Reads the durable store first (the store's sidecar tailer samples the
//!   same `lcd-latency.json` the local tap writes into a `video.latency.*` metric
//!   series + a `video.latency_source` event), falling back to a live read of
//!   `lcd-latency.json` when the store is unreachable or the probe has produced
//!   nothing. Degrades to `{"latency_ms": null, "source": "unavailable"}`.
//! - **`GET /api/v1/video/air-pipeline`** — the air-side encoder pipeline's live
//!   stats snapshot. Store-first (`video.air.*` metrics + the `video.air_state`
//!   event), with the three monotonic-clock floats the store cannot carry
//!   (`started_at` / `last_state_change_at` / `last_buffer_at`) merged from the live
//!   `air-pipeline.json` when present. Falls back wholesale to the live file read,
//!   preserving the `204` (not in use) and `503` (read error) contract.
//! - **`GET /api/video/config`** — the composite encoder + radio config snapshot.
//!   The static radio/encoder blocks come from `/etc/ados/config.yaml`; the dynamic
//!   `adaptive` / `hopping` / `link` blocks come from the controller sidecar files
//!   the wfb-side controllers persist under the runtime dir
//!   (`bitrate-controller.json` / `hop-supervisor.json` / `wfb-stats.json`),
//!   defaulting to the config-seeded stub when a sidecar is absent.
//!
//! Every read is fault-tolerant: an absent store / sidecar / config degrades to the
//! same empty/default shape the FastAPI route returns when its own source is
//! unavailable, never a 500. The routes carry no path params and never mutate, so
//! they are safe to serve natively while the snapshot/record/switch writes and the
//! camera-enumeration route (which needs the Python camera HAL) stay on the residual
//! surface.

use std::path::{Path, PathBuf};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::state::AppState;

// ---------------------------------------------------------------------------
// Runtime-dir + config seam paths.
// ---------------------------------------------------------------------------

/// The runtime dir (`ADOS_RUN_DIR`, default `/run/ados`), the same override the
/// sibling sockets + sidecars resolve under.
fn run_dir() -> PathBuf {
    PathBuf::from(std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string()))
}

/// The LCD-side SEI-latency state file (`/run/ados/lcd-latency.json`), written by
/// the local tap when the SEI latency probe is enabled. Mirrors the Python
/// `LCD_LATENCY_STATS_PATH` (with the `/run/ados/lcd-latency.json` fallback).
fn lcd_latency_path() -> PathBuf {
    run_dir().join("lcd-latency.json")
}

/// The air-side pipeline stats file (`/run/ados/air-pipeline.json`), written by
/// the encoder pipeline at 1 Hz. Mirrors `AIR_PIPELINE_STATS_PATH`.
fn air_pipeline_path() -> PathBuf {
    run_dir().join("air-pipeline.json")
}

/// The bitrate-controller snapshot file (`/run/ados/bitrate-controller.json`),
/// persisted by the closed-loop controller. Mirrors `BITRATE_CONTROLLER_JSON`.
fn bitrate_controller_path() -> PathBuf {
    run_dir().join("bitrate-controller.json")
}

/// The hop-supervisor snapshot file (`/run/ados/hop-supervisor.json`), persisted
/// by the frequency hopper. Mirrors `HOP_SUPERVISOR_JSON`.
fn hop_supervisor_path() -> PathBuf {
    run_dir().join("hop-supervisor.json")
}

/// The live wfb stats sidecar (`/run/ados/wfb-stats.json`), the link-liveness
/// source the config route's `link` block reads. Mirrors `WFB_STATS_JSON`.
fn wfb_stats_path() -> PathBuf {
    run_dir().join("wfb-stats.json")
}

/// Read a JSON snapshot file written by a sidecar producer, returning the parsed
/// object, or `None` on any read / parse failure or a non-object body. Mirrors the
/// Python `_read_state_file` / the latency live read's tolerant file load.
fn read_state_file(path: &Path) -> Option<Map<String, Value>> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

// ===========================================================================
// GET /api/video/latency
// ===========================================================================

/// `GET /api/video/latency` → the most-recent SEI-probe glass-to-glass latency.
///
/// Reads the store first; falls back to the live `lcd-latency.json` read when the
/// store is unreachable or the SEI probe has produced no samples, so the route
/// degrades to the same `{latency_ms: None, source: ...}` shape it always did.
/// Guaranteed 200.
pub async fn get_video_latency(State(state): State<AppState>) -> Json<Value> {
    if let Some(derived) = latest_video_latency(&state).await {
        return Json(derived);
    }
    Json(read_latency_live())
}

/// The `video.latency.*` metric → route-key map. Each store metric maps back to the
/// JSON key the route returns. Mirrors the Python `_LATENCY_METRICS`.
const LATENCY_METRICS: [(&str, &str); 4] = [
    ("video.latency.glass_ms", "latency_ms"),
    ("video.latency.ewma_ms", "ewma_ms"),
    ("video.latency.pipeline_ms", "pipeline_latency_ms"),
    ("video.latency.samples", "samples"),
];

/// Reconstruct the `/video/latency` route body from the store.
///
/// Maps the `video.latency.*` metrics back to the route keys and reads the
/// `source` off the latest `video.latency_source` event, falling back to `"sei"`
/// when that event is not in the window. Returns `None` when neither the
/// glass-to-glass sample nor the sample count is present (the SEI probe is disabled
/// or has produced nothing), so the route degrades to the live read. Mirrors the
/// Python `latest_video_latency`.
async fn latest_video_latency(state: &AppState) -> Option<Value> {
    let names: Vec<&str> = LATENCY_METRICS.iter().map(|(m, _)| *m).collect();
    let metrics = latest_metrics(state, &names).await;
    let glass = metric_value(metrics.as_ref(), "video.latency.glass_ms");
    let samples = metric_value(metrics.as_ref(), "video.latency.samples");
    if glass.is_none() && samples.is_none() {
        return None;
    }
    let pipeline = metric_value(metrics.as_ref(), "video.latency.pipeline_ms");
    let ewma = metric_value(metrics.as_ref(), "video.latency.ewma_ms");
    let source = latest_event_field(state, "video.latency_source", "source")
        .await
        .unwrap_or_else(|| json!("sei"));

    Some(json!({
        "latency_ms": glass.map(Value::from).unwrap_or(Value::Null),
        "ewma_ms": ewma.map(Value::from).unwrap_or(Value::Null),
        "pipeline_latency_ms": pipeline.map(Value::from).unwrap_or(Value::Null),
        "samples": samples.map(|s| json!(s as i64)).unwrap_or(Value::Null),
        "source": source,
    }))
}

/// The live SEI-latency read: the unchanged file-backed fallback, resolving the
/// runtime-dir-relative `lcd-latency.json` path. The projection itself lives in
/// [`project_latency_live`] (path-injectable for tests).
fn read_latency_live() -> Value {
    project_latency_live(&lcd_latency_path())
}

/// Project a latency state file into the route body.
///
/// Reads `lcd-latency.json` when present, projecting the latency fields, and
/// returns `{latency_ms: None, source: "unavailable"}` when the file is absent,
/// `{..., source: "read_failed"}` on a read/parse error, and `{..., source:
/// "unexpected_shape"}` for a well-formed-but-non-object body. Mirrors the Python
/// `_read_latency_live`.
fn project_latency_live(path: &Path) -> Value {
    if !path.is_file() {
        return json!({"latency_ms": null, "source": "unavailable"});
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return json!({"latency_ms": null, "source": "read_failed"}),
    };
    let blob = match serde_json::from_str::<Value>(&text) {
        Ok(v) => v,
        Err(_) => return json!({"latency_ms": null, "source": "read_failed"}),
    };
    let Some(map) = blob.as_object() else {
        return json!({"latency_ms": null, "source": "unexpected_shape"});
    };
    // `ewma_ms` prefers `latency_ewma_ms`, then `ewma_ms`. `source` defaults "sei".
    let ewma = map
        .get("latency_ewma_ms")
        .or_else(|| map.get("ewma_ms"))
        .cloned()
        .unwrap_or(Value::Null);
    let source = map
        .get("source")
        .cloned()
        .filter(|v| !v.is_null())
        .unwrap_or_else(|| json!("sei"));
    json!({
        "latency_ms": map.get("latency_ms").cloned().unwrap_or(Value::Null),
        "ewma_ms": ewma,
        "pipeline_latency_ms": map.get("pipeline_latency_ms").cloned().unwrap_or(Value::Null),
        "samples": map.get("samples").cloned().unwrap_or(Value::Null),
        "source": source,
    })
}

// ===========================================================================
// GET /api/v1/video/air-pipeline
// ===========================================================================

/// The integer `video.air.*` metric → route-key map. Mirrors `_AIR_INT_METRICS`.
const AIR_INT_METRICS: [(&str, &str); 6] = [
    ("video.air.sei_injected_count", "sei_injected_count"),
    ("video.air.udp_bytes_out", "udp_bytes_out"),
    ("video.air.restart_count", "restart_count"),
    ("video.air.tx_silent_kicks", "tx_silent_kicks"),
    ("video.air.bus_errors", "bus_errors"),
    ("video.air.updated_at_ms", "updated_at_ms"),
];

/// The float `video.air.*` metric → (route key, decimal places). Mirrors
/// `_AIR_FLOAT_METRICS`.
const AIR_FLOAT_METRICS: [(&str, &str, i32); 2] = [
    ("video.air.encoder_fps", "encoder_fps", 2),
    ("video.air.encoded_kbps", "encoded_kbps", 1),
];

/// The boolean `video.air.*` metric → route-key map. The store carries them as
/// `0.0`/`1.0`; the route casts via `value >= 0.5`. Mirrors `_AIR_BOOL_METRICS`.
const AIR_BOOL_METRICS: [(&str, &str); 2] = [
    ("video.air.encoder_hw_accel", "encoder_hw_accel"),
    ("video.air.cloud_branch_open", "cloud_branch_open"),
];

/// The three monotonic-clock floats the store cannot carry. The route fills them
/// from the live file when present, else they stay `null`. Mirrors
/// `_AIR_LIVE_ONLY_FLOATS`.
const AIR_LIVE_ONLY_FLOATS: [&str; 3] = ["started_at", "last_state_change_at", "last_buffer_at"];

/// `GET /api/v1/video/air-pipeline` → the air-side pipeline's live stats snapshot.
///
/// Reads the store first; the three monotonic-clock floats the store cannot carry
/// are filled from the live `air-pipeline.json` blob when it is present. Falls back
/// wholesale to the live file read when the store is unreachable or the air
/// pipeline is not running, preserving the `204` (not in use) and `503` (read
/// error) contract. Mirrors the Python `get_air_pipeline_status`.
pub async fn get_air_pipeline_status(State(state): State<AppState>) -> Response {
    if let Some(mut derived) = latest_air_pipeline(&state).await {
        // The store carries every field but the three monotonic floats; merge those
        // from the live file when it is present so the snapshot is whole. A live
        // read/parse error must not sink the otherwise-fresh store snapshot: the
        // three floats stay null, which is strictly better than a 503.
        if let Some(live) = read_air_pipeline_live_object() {
            if let Some(out) = derived.as_object_mut() {
                for key in AIR_LIVE_ONLY_FLOATS {
                    if let Some(v) = live.get(key) {
                        if !v.is_null() {
                            out.insert(key.to_string(), v.clone());
                        }
                    }
                }
            }
        }
        return Json(derived).into_response();
    }
    read_air_pipeline_live_response()
}

/// Reconstruct the `air-pipeline.json` route body from the store.
///
/// Maps each `video.air.*` metric back to its `AirPipelineStats.to_dict()` key
/// (re-casting integer counters, float gauges, and the two bool flags) and pulls
/// the three strings from the latest `video.air_state` event. The three
/// monotonic-clock floats are set to `null` (the store does not carry them).
/// Returns `None` when neither the metric series nor the state event is in the
/// window (the air pipeline is not running). Mirrors the Python
/// `latest_air_pipeline`.
async fn latest_air_pipeline(state: &AppState) -> Option<Value> {
    let mut names: Vec<&str> = Vec::new();
    for (m, _) in AIR_INT_METRICS {
        names.push(m);
    }
    for (m, _, _) in AIR_FLOAT_METRICS {
        names.push(m);
    }
    for (m, _) in AIR_BOOL_METRICS {
        names.push(m);
    }
    let metrics = latest_metrics(state, &names).await;
    let air = latest_event_detail(state, "video.air_state").await;
    if metrics.is_none() && air.is_none() {
        return None;
    }

    let mut out = Map::new();
    // Strings from the state event (empty defaults match the dataclass).
    let air_obj = air.as_ref();
    out.insert(
        "camera_source".to_string(),
        json!(event_str(air_obj, "camera_source", "")),
    );
    out.insert(
        "encoder_name".to_string(),
        json!(event_str(air_obj, "encoder_name", "")),
    );
    out.insert(
        "pipeline_state".to_string(),
        json!(event_str(air_obj, "pipeline_state", "idle")),
    );

    for (name, key) in AIR_INT_METRICS {
        let value = metric_value(metrics.as_ref(), name);
        out.insert(key.to_string(), json!(value.map(|v| v as i64).unwrap_or(0)));
    }
    for (name, key, ndigits) in AIR_FLOAT_METRICS {
        let value = metric_value(metrics.as_ref(), name);
        out.insert(
            key.to_string(),
            json!(value.map(|v| round_half_even(v, ndigits)).unwrap_or(0.0)),
        );
    }
    for (name, key) in AIR_BOOL_METRICS {
        let value = metric_value(metrics.as_ref(), name);
        out.insert(
            key.to_string(),
            json!(value.map(|v| v >= 0.5).unwrap_or(false)),
        );
    }

    // The monotonic-clock floats carry no cross-process meaning; the route fills
    // them from the live file when it can, else they stay null.
    for key in AIR_LIVE_ONLY_FLOATS {
        out.insert(key.to_string(), Value::Null);
    }
    Some(Value::Object(out))
}

/// The live air-pipeline read producing an axum `Response`, resolving the
/// runtime-dir-relative `air-pipeline.json` path. The projection lives in
/// [`project_air_pipeline_live`] (path-injectable for tests).
fn read_air_pipeline_live_response() -> Response {
    project_air_pipeline_live(&air_pipeline_path())
}

/// Project an air-pipeline state file into an axum `Response`: the parsed dict as a
/// 200, a 204 when the file is absent or not a dict (the legacy stream owns the
/// pipeline), and a `{"detail"}` 503 on a read/parse error. This is the unchanged
/// file-backed fallback the store-first path falls through to. Mirrors the Python
/// `_read_air_pipeline_live_blob`.
fn project_air_pipeline_live(path: &Path) -> Response {
    if !path.exists() {
        return StatusCode::NO_CONTENT.into_response();
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => {
            return crate::routes::detail(
                StatusCode::SERVICE_UNAVAILABLE,
                "air pipeline stats unavailable",
            );
        }
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Json(Value::Object(map)).into_response(),
        // A well-formed-but-non-object body → 204 (the `if not isinstance(blob,
        // dict): return Response(204)` branch).
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => crate::routes::detail(
            StatusCode::SERVICE_UNAVAILABLE,
            "air pipeline stats unavailable",
        ),
    }
}

/// The live air-pipeline blob read returning just the object (for the float-merge
/// on the store-first path). Returns `None` when the file is absent / unparseable /
/// non-object — the store-first merge only wants the live floats and never sinks on
/// a read error here. Mirrors the merge branch's `_read_air_pipeline_live_blob`
/// call wrapped in `try: ... except: live = None`.
fn read_air_pipeline_live_object() -> Option<Map<String, Value>> {
    read_state_file(&air_pipeline_path())
}

// ===========================================================================
// GET /api/video/config
// ===========================================================================

/// `GET /api/video/config` → the composite encoder + radio + adaptive + hopping +
/// link snapshot.
///
/// The static `radio` / `encoder` blocks are projected from `/etc/ados/config.yaml`
/// (the loaded config object's `video.wfb` + `video.camera` slices, with the
/// Pydantic field defaults applied for any absent field). The dynamic `adaptive` /
/// `hopping` / `link` blocks come from the controller sidecar files; an absent
/// sidecar degrades each to the config-seeded stub. Guaranteed 200. Mirrors the
/// FastAPI `get_video_config` on the multi-process path (where the in-process
/// managers are absent and every dynamic block reads its sidecar).
pub async fn get_video_config() -> Json<Value> {
    let cfg = VideoConfig::load();
    let wfb = &cfg.video.wfb;
    let camera = &cfg.video.camera;

    let radio = json!({
        "channel": wfb.channel,
        "band": wfb.band,
        "mcs_index": wfb.mcs_index,
        "fec_k": wfb.fec_k,
        "fec_n": wfb.fec_n,
        "tx_power_dbm": wfb.tx_power_dbm,
        "preset": wfb.wfb_link_preset,
    });
    let encoder = json!({
        "bitrate_kbps": camera.bitrate_kbps,
        "width": camera.width,
        "height": camera.height,
        "fps": camera.fps,
        "codec": camera.codec,
    });

    // adaptive: `{available}` from config, with the bitrate-controller snapshot
    // merged over it when the sidecar is present.
    let mut adaptive = Map::new();
    adaptive.insert("available".to_string(), json!(wfb.adaptive_bitrate_enabled));
    if let Some(snap) = read_state_file(&bitrate_controller_path()) {
        for (k, v) in snap {
            adaptive.insert(k, v);
        }
    }

    // hopping: the supervisor snapshot, or a config-seeded stub when absent.
    let hopping = match read_state_file(&hop_supervisor_path()) {
        Some(snap) => Value::Object(snap),
        None => json!({
            "enabled": wfb.auto_hop_enabled,
            "band": wfb.band,
            "hop_period_seconds": wfb.hop_period_seconds,
            "history": [],
            "last_hop_at": 0.0,
        }),
    };

    let link = link_snapshot(wfb.channel, &wfb_stats_path());

    Json(json!({
        "radio": radio,
        "encoder": encoder,
        "adaptive": Value::Object(adaptive),
        "hopping": hopping,
        "link": link,
    }))
}

/// The live radio-link liveness block the GCS Video Link panel reads from
/// `config.link.*`. The in-process wfb manager is absent on this native front, so
/// the values come from the `wfb-stats.json` sidecar the radio mirrors; `channel`
/// falls back to the configured value when the sidecar has no value yet. Every
/// field is present (a `null` placeholder when unknown) so the panel never sees a
/// missing key. Mirrors the Python `_link_snapshot` multi-process branch.
fn link_snapshot(config_channel: i64, stats_path: &Path) -> Value {
    const FIELDS: [&str; 7] = [
        "tx_bytes_per_s",
        "valid_rx_packets_per_s",
        "video_inbound_bytes_per_s",
        "rx_silent_seconds",
        "channel_locked",
        "acquire_state",
        "channel",
    ];
    let mut link = Map::new();
    for f in FIELDS {
        link.insert(f.to_string(), Value::Null);
    }
    if let Some(status) = read_state_file(stats_path) {
        for f in FIELDS {
            if let Some(v) = status.get(f) {
                if !v.is_null() {
                    link.insert(f.to_string(), v.clone());
                }
            }
        }
    }
    // Channel falls back to the configured value so the panel always has a number
    // even before the first stats line lands.
    if link.get("channel").map(Value::is_null).unwrap_or(true) {
        link.insert("channel".to_string(), json!(config_channel));
    }
    Value::Object(link)
}

// ---------------------------------------------------------------------------
// Config seam: the `video.wfb` + `video.camera` slices, with Pydantic defaults.
// ---------------------------------------------------------------------------

/// The `video.wfb` fields the config route projects. Each field carries the Python
/// `WfbConfig` default so an absent field reads the same value the loaded Python
/// config object would (which fills every field from its model defaults).
#[derive(Debug, Clone, Deserialize)]
struct WfbConfigSection {
    #[serde(default = "default_channel")]
    channel: i64,
    #[serde(default = "default_band")]
    band: String,
    #[serde(default = "default_mcs_index")]
    mcs_index: i64,
    #[serde(default = "default_fec_k")]
    fec_k: i64,
    #[serde(default = "default_fec_n")]
    fec_n: i64,
    #[serde(default = "default_tx_power_dbm")]
    tx_power_dbm: i64,
    #[serde(default = "default_wfb_link_preset")]
    wfb_link_preset: String,
    #[serde(default = "default_true")]
    adaptive_bitrate_enabled: bool,
    #[serde(default = "default_true")]
    auto_hop_enabled: bool,
    #[serde(default = "default_hop_period_seconds")]
    hop_period_seconds: i64,
}

fn default_channel() -> i64 {
    149
}
fn default_band() -> String {
    "u-nii-3".to_string()
}
fn default_mcs_index() -> i64 {
    1
}
fn default_fec_k() -> i64 {
    8
}
fn default_fec_n() -> i64 {
    12
}
fn default_tx_power_dbm() -> i64 {
    5
}
fn default_wfb_link_preset() -> String {
    "conservative".to_string()
}
fn default_true() -> bool {
    true
}
fn default_hop_period_seconds() -> i64 {
    60
}

impl Default for WfbConfigSection {
    fn default() -> Self {
        Self {
            channel: default_channel(),
            band: default_band(),
            mcs_index: default_mcs_index(),
            fec_k: default_fec_k(),
            fec_n: default_fec_n(),
            tx_power_dbm: default_tx_power_dbm(),
            wfb_link_preset: default_wfb_link_preset(),
            adaptive_bitrate_enabled: default_true(),
            auto_hop_enabled: default_true(),
            hop_period_seconds: default_hop_period_seconds(),
        }
    }
}

/// The `video.camera` fields the encoder block projects, with the Python
/// `CameraConfig` defaults.
#[derive(Debug, Clone, Deserialize)]
struct CameraConfigSection {
    #[serde(default = "default_bitrate_kbps")]
    bitrate_kbps: i64,
    #[serde(default = "default_width")]
    width: i64,
    #[serde(default = "default_height")]
    height: i64,
    #[serde(default = "default_fps")]
    fps: i64,
    #[serde(default = "default_codec")]
    codec: String,
}

fn default_bitrate_kbps() -> i64 {
    4000
}
fn default_width() -> i64 {
    1280
}
fn default_height() -> i64 {
    720
}
fn default_fps() -> i64 {
    30
}
fn default_codec() -> String {
    "h264".to_string()
}

impl Default for CameraConfigSection {
    fn default() -> Self {
        Self {
            bitrate_kbps: default_bitrate_kbps(),
            width: default_width(),
            height: default_height(),
            fps: default_fps(),
            codec: default_codec(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct VideoSection {
    #[serde(default)]
    wfb: WfbConfigSection,
    #[serde(default)]
    camera: CameraConfigSection,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct VideoConfig {
    #[serde(default)]
    video: VideoSection,
}

impl VideoConfig {
    /// Load the `video.wfb` + `video.camera` slices from the config path
    /// (`ADOS_CONFIG`, default `/etc/ados/config.yaml`). A missing or unparseable
    /// file yields the all-defaults slice, so the route still answers a usable body
    /// with every field at its Python default.
    fn load() -> Self {
        let path = std::env::var("ADOS_CONFIG")
            .unwrap_or_else(|_| crate::config::CONFIG_YAML.to_string());
        Self::load_from(Path::new(&path))
    }

    fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_norway::from_str(&text).unwrap_or_default(),
            Err(_) => VideoConfig::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// logd query seam: HTTP-over-UDS reads of the store's /v1 metrics + events.
// ---------------------------------------------------------------------------

/// The newest value (as a JSON value) per named metric from a recent `metrics`
/// page, newest-wins. Returns `None` when the store is unreachable; a name not seen
/// in the window is simply absent from the map. Mirrors the Python `latest_metrics`
/// over the named set (the route only reads the value, so tags/ts are dropped).
async fn latest_metrics(state: &AppState, names: &[&str]) -> Option<Map<String, Value>> {
    let rows = logd_query_rows(state, "metrics", 200, None).await?;
    let mut out: Map<String, Value> = Map::new();
    for row in rows {
        let Some(obj) = row.as_object() else { continue };
        let Some(metric) = obj.get("metric").and_then(Value::as_str) else {
            continue;
        };
        if names.contains(&metric) && !out.contains_key(metric) {
            out.insert(
                metric.to_string(),
                obj.get("value").cloned().unwrap_or(Value::Null),
            );
        }
    }
    Some(out)
}

/// The newest numeric value for `name` from a merged metric map, or `None` if the
/// name is absent / non-numeric / a bool. Mirrors the Python `_metric_value` (which
/// excludes bools and casts to float).
fn metric_value(metrics: Option<&Map<String, Value>>, name: &str) -> Option<f64> {
    let value = metrics?.get(name)?;
    match value {
        // A JSON bool is not a `Number`; this rejects it the way the Python helper's
        // explicit `isinstance(value, bool)` check does.
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

/// The `detail` object of the newest events row whose `kind` matches, or `None`.
/// Mirrors the Python `_latest_event`: filter the events table to the kind
/// server-side (`event_kind`), re-check the kind client-side, and return the
/// `detail` (an empty object when the detail is absent / non-object).
async fn latest_event_detail(state: &AppState, kind: &str) -> Option<Map<String, Value>> {
    let rows = logd_query_rows(state, "events", 50, Some(kind)).await?;
    for row in rows {
        let Some(obj) = row.as_object() else { continue };
        if obj.get("kind").and_then(Value::as_str) == Some(kind) {
            return Some(match obj.get("detail") {
                Some(Value::Object(d)) => d.clone(),
                _ => Map::new(),
            });
        }
    }
    None
}

/// The value of `field` on the latest event of `kind`, or `None`. A convenience
/// over [`latest_event_detail`] for the latency route's `source` read.
async fn latest_event_field(state: &AppState, kind: &str, field: &str) -> Option<Value> {
    let detail = latest_event_detail(state, kind).await?;
    detail.get(field).cloned()
}

/// A string field off an event-detail object, with a default. Mirrors the Python
/// `(air or {}).get(key) or default`: a missing key, a null, or an empty string all
/// fall back to the default.
fn event_str(detail: Option<&Map<String, Value>>, key: &str, default: &str) -> String {
    detail
        .and_then(|d| d.get(key))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(default)
        .to_string()
}

/// Page the store's `/v1/query` for one row kind, returning the `data` array, or
/// `None` when the store is unreachable / the response is an error / does not
/// parse. An optional `event_kind` filters the events table server-side. Mirrors
/// the read side of the Python `query_rows`.
async fn logd_query_rows(
    state: &AppState,
    kind: &str,
    limit: i64,
    event_kind: Option<&str>,
) -> Option<Vec<Value>> {
    let mut params: Vec<(&str, String)> = vec![
        ("kind", kind.to_string()),
        ("limit", limit.to_string()),
    ];
    if let Some(ek) = event_kind {
        params.push(("event_kind", ek.to_string()));
    }
    let query = encode_query(&params);
    let path = format!("/v1/query?{query}");
    let (status, body) = logd_get(state, &path).await.ok()?;
    if status >= 400 {
        return None;
    }
    let parsed: Value = serde_json::from_slice(&body).ok()?;
    parsed
        .get("data")
        .and_then(Value::as_array)
        .map(|a| a.to_vec())
}

/// A minimal HTTP/1.1 `GET` over the logging-store query Unix socket, returning the
/// status code + the decoded body. The socket path comes from the app state's logd
/// client so a test redirects it. `Connection: close` reads the body to EOF; a
/// chunked body is de-chunked. Bounded so a runaway response cannot exhaust memory.
async fn logd_get(state: &AppState, path: &str) -> std::io::Result<(u16, Vec<u8>)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A hard ceiling on the response read; a normal metrics/events page is a few
    /// KiB, so this only guards a runaway body.
    const MAX_READ_BYTES: usize = 4 * 1024 * 1024;

    let socket = state.logd.socket_path();
    let mut stream = tokio::net::UnixStream::connect(socket).await?;
    let head = format!("GET {path} HTTP/1.1\r\nHost: logd\r\nConnection: close\r\n\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.flush().await?;

    let mut raw = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break; // EOF (Connection: close).
        }
        if raw.len() + n > MAX_READ_BYTES {
            return Err(std::io::Error::other("logd response too large"));
        }
        raw.extend_from_slice(&buf[..n]);
    }
    parse_http_response(&raw)
}

/// Split a raw HTTP/1.1 response into the status code + decoded body. De-chunks a
/// `Transfer-Encoding: chunked` body; otherwise returns the body after the header
/// terminator as-is.
fn parse_http_response(raw: &[u8]) -> std::io::Result<(u16, Vec<u8>)> {
    let sep = b"\r\n\r\n";
    let split = raw
        .windows(sep.len())
        .position(|w| w == sep)
        .ok_or_else(|| std::io::Error::other("malformed http response (no header terminator)"))?;
    let head = &raw[..split];
    let body = &raw[split + sep.len()..];

    let head_str = String::from_utf8_lossy(head);
    let status = head_str
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| std::io::Error::other("malformed http status line"))?;

    let chunked = head_str
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked");
    let body = if chunked {
        de_chunk(body)
    } else {
        body.to_vec()
    };
    Ok((status, body))
}

/// De-chunk a `Transfer-Encoding: chunked` body: `<hexlen>\r\n<data>\r\n` repeated
/// until a zero-length chunk.
fn de_chunk(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(crlf) = rest.windows(2).position(|w| w == b"\r\n") {
        let len_line = &rest[..crlf];
        let len = usize::from_str_radix(String::from_utf8_lossy(len_line).trim(), 16).unwrap_or(0);
        if len == 0 {
            break;
        }
        let data_start = crlf + 2;
        if rest.len() < data_start + len {
            out.extend_from_slice(&rest[data_start..]);
            break;
        }
        out.extend_from_slice(&rest[data_start..data_start + len]);
        let next = data_start + len;
        rest = if rest.len() >= next + 2 {
            &rest[next + 2..]
        } else {
            &[]
        };
    }
    out
}

// ---------------------------------------------------------------------------
// Small shared helpers.
// ---------------------------------------------------------------------------

/// Percent-encode a query-parameter list into a `key=value&...` string. Only the
/// characters the store's query values use appear, so a conservative
/// reserved-character escape is sufficient.
fn encode_query(params: &[(&str, String)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Conservative percent-encoding: pass through the unreserved set
/// (`A-Za-z0-9-._~`) verbatim and percent-encode every other byte.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Round to `ndigits` decimal places with round-half-to-even (banker's rounding),
/// matching the Python built-in `round(value, ndigits)` the air-pipeline float
/// gauges use. The store carries these as floats, so the two ends agree on the same
/// rounded value.
fn round_half_even(value: f64, ndigits: i32) -> f64 {
    let factor = 10f64.powi(ndigits);
    let scaled = value * factor;
    let floor = scaled.floor();
    let diff = scaled - floor;
    let rounded = if (diff - 0.5).abs() < f64::EPSILON {
        // Exactly halfway: round to the nearest even integer.
        if (floor as i64) % 2 == 0 {
            floor
        } else {
            floor + 1.0
        }
    } else {
        scaled.round()
    };
    rounded / factor
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- /api/video/latency -----

    #[test]
    fn latency_live_of_an_absent_file_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let out = project_latency_live(&dir.path().join("lcd-latency.json"));
        assert_eq!(out, json!({"latency_ms": null, "source": "unavailable"}));
    }

    #[test]
    fn latency_live_projects_the_file_fields_with_ewma_preference() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lcd-latency.json");
        std::fs::write(
            &path,
            r#"{"latency_ms": 82.5, "latency_ewma_ms": 80.0, "ewma_ms": 999,
                "pipeline_latency_ms": 12.0, "samples": 30}"#,
        )
        .unwrap();
        let out = project_latency_live(&path);
        // The golden latency body the GCS reads (live-file path). `latency_ewma_ms`
        // wins over `ewma_ms`; `source` defaults to "sei" when absent.
        let want = json!({
            "latency_ms": 82.5,
            "ewma_ms": 80.0,
            "pipeline_latency_ms": 12.0,
            "samples": 30,
            "source": "sei",
        });
        assert_eq!(out, want);
    }

    #[test]
    fn latency_live_of_a_non_object_is_unexpected_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lcd-latency.json");
        std::fs::write(&path, "[1,2,3]").unwrap();
        let out = project_latency_live(&path);
        assert_eq!(out, json!({"latency_ms": null, "source": "unexpected_shape"}));
    }

    // ----- /api/video/config -----

    #[test]
    fn config_of_an_empty_yaml_is_the_pydantic_default_shape() {
        // An empty config maps to the loaded Python config object's field defaults
        // for radio + encoder; the absent sidecars give the config-seeded stubs.
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.yaml");
        std::fs::write(&cfg_path, "agent:\n  profile: drone\n").unwrap();

        let cfg = VideoConfig::load_from(&cfg_path);
        let wfb = &cfg.video.wfb;
        let camera = &cfg.video.camera;
        assert_eq!(wfb.channel, 149);
        assert_eq!(wfb.band, "u-nii-3");
        assert_eq!(wfb.mcs_index, 1);
        assert_eq!(wfb.fec_k, 8);
        assert_eq!(wfb.fec_n, 12);
        assert_eq!(wfb.tx_power_dbm, 5);
        assert_eq!(wfb.wfb_link_preset, "conservative");
        assert!(wfb.adaptive_bitrate_enabled);
        assert!(wfb.auto_hop_enabled);
        assert_eq!(wfb.hop_period_seconds, 60);
        assert_eq!(camera.bitrate_kbps, 4000);
        assert_eq!(camera.width, 1280);
        assert_eq!(camera.height, 720);
        assert_eq!(camera.fps, 30);
        assert_eq!(camera.codec, "h264");

        // The hopping/link blocks are the config-seeded stubs with no sidecars
        // (point the sidecar paths at absent files in the tempdir).
        let hopping = match read_state_file(&dir.path().join("hop-supervisor.json")) {
            Some(snap) => Value::Object(snap),
            None => json!({
                "enabled": wfb.auto_hop_enabled,
                "band": wfb.band,
                "hop_period_seconds": wfb.hop_period_seconds,
                "history": [],
                "last_hop_at": 0.0,
            }),
        };
        assert_eq!(
            hopping,
            json!({
                "enabled": true,
                "band": "u-nii-3",
                "hop_period_seconds": 60,
                "history": [],
                "last_hop_at": 0.0,
            })
        );
        let link = link_snapshot(wfb.channel, &dir.path().join("wfb-stats.json"));
        // Every field present; channel falls back to the configured value.
        assert_eq!(link["channel"], json!(149));
        assert_eq!(link["acquire_state"], Value::Null);
        assert_eq!(link["tx_bytes_per_s"], Value::Null);
    }

    #[test]
    fn config_reads_explicit_radio_and_encoder_fields() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.yaml");
        std::fs::write(
            &cfg_path,
            "video:\n  wfb:\n    channel: 165\n    mcs_index: 3\n    fec_k: 4\n    fec_n: 8\n    tx_power_dbm: 12\n    wfb_link_preset: aggressive\n  camera:\n    bitrate_kbps: 6000\n    width: 1920\n    height: 1080\n    fps: 60\n    codec: h265\n",
        )
        .unwrap();
        let cfg = VideoConfig::load_from(&cfg_path);
        assert_eq!(cfg.video.wfb.channel, 165);
        assert_eq!(cfg.video.wfb.mcs_index, 3);
        assert_eq!(cfg.video.wfb.fec_k, 4);
        assert_eq!(cfg.video.wfb.fec_n, 8);
        assert_eq!(cfg.video.wfb.tx_power_dbm, 12);
        assert_eq!(cfg.video.wfb.wfb_link_preset, "aggressive");
        assert_eq!(cfg.video.camera.bitrate_kbps, 6000);
        assert_eq!(cfg.video.camera.codec, "h265");
        // Absent fields still take their Python defaults.
        assert_eq!(cfg.video.wfb.band, "u-nii-3");
        assert_eq!(cfg.video.camera.fps, 60);
    }

    #[test]
    fn link_snapshot_merges_present_stats_over_null_placeholders() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wfb-stats.json");
        std::fs::write(
            &path,
            r#"{"tx_bytes_per_s": 12345, "acquire_state": "locked", "channel": 149,
                "channel_locked": true, "extra": "ignored"}"#,
        )
        .unwrap();
        let link = link_snapshot(36, &path);
        assert_eq!(link["tx_bytes_per_s"], json!(12345));
        assert_eq!(link["acquire_state"], json!("locked"));
        assert_eq!(link["channel_locked"], json!(true));
        // The sidecar's channel wins over the config fallback.
        assert_eq!(link["channel"], json!(149));
        // A field not in the stats file stays null.
        assert_eq!(link["video_inbound_bytes_per_s"], Value::Null);
        // Only the seven contract fields are present (the extra is dropped).
        assert_eq!(link.as_object().unwrap().len(), 7);
    }

    // ----- /api/v1/video/air-pipeline -----

    #[test]
    fn air_pipeline_live_of_an_absent_file_is_204() {
        let dir = tempfile::tempdir().unwrap();
        let resp = project_air_pipeline_live(&dir.path().join("air-pipeline.json"));
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[test]
    fn air_pipeline_live_of_a_present_object_is_200() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("air-pipeline.json");
        std::fs::write(
            &path,
            r#"{"pipeline_state": "running", "encoder_fps": 30.0}"#,
        )
        .unwrap();
        let resp = project_air_pipeline_live(&path);
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn air_pipeline_live_of_a_non_object_body_is_204() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("air-pipeline.json");
        std::fs::write(&path, "42").unwrap();
        let resp = project_air_pipeline_live(&path);
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    // ----- shared helpers -----

    #[test]
    fn metric_value_excludes_bools_and_non_numbers() {
        let mut m = Map::new();
        m.insert("a".to_string(), json!(12.5));
        m.insert("b".to_string(), json!(true));
        m.insert("c".to_string(), json!("x"));
        assert_eq!(metric_value(Some(&m), "a"), Some(12.5));
        assert_eq!(metric_value(Some(&m), "b"), None); // bool excluded
        assert_eq!(metric_value(Some(&m), "c"), None); // string excluded
        assert_eq!(metric_value(Some(&m), "absent"), None);
        assert_eq!(metric_value(None, "a"), None);
    }

    #[test]
    fn event_str_falls_back_on_missing_null_and_empty() {
        let mut d = Map::new();
        d.insert("present".to_string(), json!("camX"));
        d.insert("empty".to_string(), json!(""));
        d.insert("nul".to_string(), Value::Null);
        assert_eq!(event_str(Some(&d), "present", "def"), "camX");
        assert_eq!(event_str(Some(&d), "empty", "def"), "def");
        assert_eq!(event_str(Some(&d), "nul", "def"), "def");
        assert_eq!(event_str(Some(&d), "absent", "def"), "def");
        assert_eq!(event_str(None, "present", "idle"), "idle");
    }

    #[test]
    fn round_half_even_matches_python_round() {
        // Python round(value, ndigits) is round-half-to-even.
        assert_eq!(round_half_even(30.0, 2), 30.0);
        assert_eq!(round_half_even(29.456, 1), 29.5);
        assert_eq!(round_half_even(29.444, 1), 29.4);
        // Halfway cases round to even: 0.5->0, 1.5->2, 2.5->2.
        assert_eq!(round_half_even(0.5, 0), 0.0);
        assert_eq!(round_half_even(1.5, 0), 2.0);
        assert_eq!(round_half_even(2.5, 0), 2.0);
    }

    #[test]
    fn percent_encode_escapes_reserved_chars() {
        assert_eq!(percent_encode("video.air.encoder_fps"), "video.air.encoder_fps");
        assert_eq!(percent_encode("metrics"), "metrics");
        assert_eq!(percent_encode("a b"), "a%20b");
    }

    #[test]
    fn de_chunk_reassembles_a_chunked_body() {
        let chunked = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        assert_eq!(de_chunk(chunked), b"hello world");
    }

    #[test]
    fn parse_http_response_reads_status_and_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
        let (status, body) = parse_http_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"{}");
    }

    #[test]
    fn read_state_file_tolerates_absent_and_non_object() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_state_file(&dir.path().join("absent.json")).is_none());
        std::fs::write(dir.path().join("arr.json"), "[1,2]").unwrap();
        assert!(read_state_file(&dir.path().join("arr.json")).is_none());
        std::fs::write(dir.path().join("obj.json"), r#"{"k":1}"#).unwrap();
        assert_eq!(
            read_state_file(&dir.path().join("obj.json")).unwrap()["k"],
            json!(1)
        );
    }
}
