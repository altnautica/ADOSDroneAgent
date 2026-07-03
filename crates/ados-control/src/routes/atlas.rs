//! ADOS Atlas capture-control routes: the GCS drives per-drone world-model
//! capture over the LAN through these, so an operator never hand-edits a config
//! file. This is the RUST-FIRST home for the control plane (native `ados-control`
//! routes, not a residual-Python surface).
//!
//! - **`GET /api/atlas/readiness`** — the drone-local facts the GCS needs to show
//!   whether this drone can build a world model and whether it is capturing now:
//!   `enabled` + `cameras_configured` + `capture_profile` + `pose_source` from the
//!   agent config, plus the live `state` / `capturing` / `session_id` /
//!   `service_running` read from the capture service's control socket, plus the
//!   `compute_node_id` the egress forwarder is actively streaming to (folded from
//!   its handoff sidecar, freshness-gated so a dead forwarder never lingers).
//!   Compute-node reachability probing is the GCS's own concern (it already knows
//!   its paired workstation nodes), so it is not re-probed here — only the node the
//!   forwarder resolved is surfaced.
//! - **`PUT /api/atlas/config`** — enable/disable capture on this drone plus the
//!   capture profile and camera set. Writes the `atlas:` block into
//!   `/etc/ados/config.yaml` with a surgical merge that preserves every other key
//!   (the same helpers the vision-detector write uses), then restarts `ados-atlas`
//!   so the change takes effect. This is the per-drone enable the GCS calls; every
//!   field written is one the capture service reads.
//! - **`POST /api/atlas/capture/{start,stop,pause,resume}`** — drive the live
//!   capture session. `stop` finalizes + bags the session, which is what triggers
//!   the compute node to reconstruct. Each forwards to the control socket and
//!   returns the resulting capture status; an unreachable service is a 503.
//!
//! ## Auth posture
//!
//! The config + capture routes are writes, so they sit in the native route set
//! and the LAN edge requires the pairing key when paired (the same posture as
//! `/api/command` and `/api/vision/detector`); readiness is a read served with
//! the same native posture as the compute-status read.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use ados_protocol::atlas::{
    AtlasForwardStatus, CaptureState, CaptureStatus, ATLAS_FORWARD_SIDECAR,
    ATLAS_FORWARD_SIDECAR_VERSION,
};
use ados_protocol::sidecar::check_sidecar_version;

use crate::ipc::atlas_control_client::{AtlasControlClient, AtlasControlError};
use crate::routes::detail;
use crate::routes::service_control::restart_unit;
use crate::routes::wfb_pair_write::write_atomic;

/// The unit that runs the capture service; restarting it applies a config change.
const ATLAS_UNIT: &str = "ados-atlas";

/// The capture profiles the service accepts, matching the `CaptureProfile` enum's
/// serde repr in the capture crate (orbit / lawnmower / freeform / inspection).
/// An out-of-set value is rejected (400) rather than written verbatim, so
/// `config.yaml` never carries a profile the capture service would fail to parse.
const VALID_CAPTURE_PROFILES: [&str; 4] = ["orbit", "lawnmower", "freeform", "inspection"];

/// The camera roles the capture service accepts, matching the `CameraRole` enum's
/// serde repr (lowercase). An out-of-set role is rejected (400) rather than
/// written verbatim: an unknown role fails the whole `atlas:` block's parse, which
/// would silently default the block to disabled, so it is caught at the API edge.
const VALID_CAMERA_ROLES: [&str; 7] = ["primary", "aux", "down", "left", "right", "back", "up"];

/// A forwarder handoff not re-written within this window is treated as absent, so a
/// dead forwarder never keeps a stale compute node on the readiness surface
/// (operating rule 44). Comfortably larger than the forwarder's refresh cadence.
const FORWARD_STALE: Duration = Duration::from_secs(15);

/// The agent config path (`ADOS_CONFIG`, default `/etc/ados/config.yaml`), the
/// same resolution the sibling write routes use.
fn config_yaml_path() -> PathBuf {
    PathBuf::from(
        std::env::var("ADOS_CONFIG").unwrap_or_else(|_| crate::config::CONFIG_YAML.to_string()),
    )
}

/// The drone-local view of the `atlas:` config block the readiness route projects.
struct AtlasConfigView {
    enabled: bool,
    capture_profile: String,
    cameras_configured: u32,
    pose_tier: String,
    profile: String,
}

/// Read the agent config's `atlas:` block into a view, degrading every field to a
/// sane default when the file, the block, or a key is absent (a fresh drone has
/// no `atlas:` block yet: disabled, no cameras). Never fails.
fn read_atlas_config_view(config_path: &Path) -> AtlasConfigView {
    use serde_norway::Value as Yaml;

    let doc: Option<Yaml> = std::fs::read_to_string(config_path)
        .ok()
        .and_then(|t| serde_norway::from_str::<Yaml>(&t).ok())
        .filter(|v| v.is_mapping());

    let profile = doc
        .as_ref()
        .and_then(|d| d.get("agent"))
        .and_then(|a| a.get("profile"))
        .and_then(|p| p.as_str())
        .unwrap_or("drone")
        .to_string();

    let atlas = doc.as_ref().and_then(|d| d.get("atlas"));

    let enabled = atlas
        .and_then(|a| a.get("enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let capture_profile = atlas
        .and_then(|a| a.get("capture_profile"))
        .and_then(|v| v.as_str())
        .unwrap_or("freeform")
        .to_string();
    let pose_tier = atlas
        .and_then(|a| a.get("pose_tier"))
        .and_then(|v| v.as_str())
        .unwrap_or("auto")
        .to_string();
    let cameras_configured = atlas
        .and_then(|a| a.get("cameras"))
        .and_then(|v| v.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter(|c| c.get("enabled").and_then(|e| e.as_bool()).unwrap_or(false))
                .count() as u32
        })
        .unwrap_or(0);

    AtlasConfigView {
        enabled,
        capture_profile,
        cameras_configured,
        pose_tier,
        profile,
    }
}

/// Map the configured pose tier to the pose source the drone will use. `auto` and
/// `local` resolve to the always-available flight-controller VIO; `offload`/
/// `hybrid` carry through so the GCS can show an NPU-less drone's offloaded pose.
fn pose_source_for(tier: &str) -> &'static str {
    match tier {
        "offload" => "offloaded_slam",
        "hybrid" => "hybrid",
        _ => "local_vio",
    }
}

/// The stable wire string for a capture state, matching the enum's serde repr the
/// GCS compares against.
fn capture_state_str(state: CaptureState) -> &'static str {
    match state {
        CaptureState::Idle => "idle",
        CaptureState::Capturing => "capturing",
        CaptureState::Paused => "paused",
        CaptureState::Finalizing => "finalizing",
        CaptureState::Bagged => "bagged",
    }
}

/// Read the forwarder handoff at `path` if it exists AND was written within
/// [`FORWARD_STALE`] of `now`. A stale file (a dead forwarder whose tmpfs file
/// persists) is treated as absent so the readiness surface never reports a compute
/// node that is gone (operating rule 44). A future/unreadable mtime counts as
/// fresh. Best-effort: any I/O or parse error yields `None`.
fn read_fresh_forward_status(path: &Path, now: SystemTime) -> Option<AtlasForwardStatus> {
    let meta = std::fs::metadata(path).ok()?;
    if let Ok(mtime) = meta.modified() {
        if let Ok(age) = now.duration_since(mtime) {
            if age > FORWARD_STALE {
                return None;
            }
        }
    }
    let text = std::fs::read_to_string(path).ok()?;
    let status = serde_json::from_str::<AtlasForwardStatus>(&text).ok()?;
    // Best-effort drift signal: warn (never reject) if the handoff was written by a
    // differently-versioned forwarder, then fold it in anyway.
    check_sidecar_version(
        "atlas-forward",
        status.version,
        ATLAS_FORWARD_SIDECAR_VERSION,
    );
    Some(status)
}

/// `GET /api/atlas/readiness` → the drone-local capture readiness + live state.
///
/// Resolves the config path, the live capture status (the control socket, which
/// errors to `None` = idle when the service is down), and the forwarder handoff
/// sidecar, and hands all three to the pure [`build_atlas_readiness`] core, so the
/// projection is testable with an explicit config path, an injected status, and an
/// explicit sidecar path — no env and no live socket.
pub async fn get_atlas_readiness() -> Response {
    // The live session state comes from the capture service's control socket. If
    // it is unreachable, the service is not running (atlas disabled, or no
    // cameras), so the session is idle.
    let live = AtlasControlClient::default_socket().status().await.ok();
    build_atlas_readiness(&config_yaml_path(), live, Path::new(ATLAS_FORWARD_SIDECAR))
}

/// The pure core of [`get_atlas_readiness`]: project the drone-local `atlas:`
/// config view + the (optional) live capture status + the forwarder handoff into
/// the readiness body. Config-, status-, and sidecar-path-injectable so tests
/// exercise it deterministically without touching `ADOS_CONFIG` / `ADOS_RUN_DIR`
/// or standing up a control socket.
fn build_atlas_readiness(
    config_path: &Path,
    live: Option<CaptureStatus>,
    forward_sidecar_path: &Path,
) -> Response {
    let view = read_atlas_config_view(config_path);

    let (service_running, capturing, state, session_id, camera_count, keyframes, ingest_rate_hz) =
        match &live {
            Some(s) => (
                true,
                matches!(s.state, CaptureState::Capturing | CaptureState::Paused),
                capture_state_str(s.state),
                (!s.session_id.is_empty()).then(|| s.session_id.clone()),
                s.camera_count,
                s.keyframes,
                s.ingest_rate_hz,
            ),
            None => (false, false, "idle", None, view.cameras_configured, 0, 0.0),
        };

    // The compute node the egress forwarder is actively streaming to is known only
    // by the forwarder, which writes it to the handoff sidecar. Surface it here
    // (freshness-gated) so the GCS shows the real node rather than null while a
    // capture is forwarding; a stale/absent handoff yields `null`.
    let compute_node_id = read_fresh_forward_status(forward_sidecar_path, SystemTime::now())
        .and_then(|f| f.compute_node_id);

    Json(json!({
        "enabled": view.enabled,
        "profile": view.profile,
        "capture_profile": view.capture_profile,
        "cameras_configured": view.cameras_configured,
        "pose_source": pose_source_for(&view.pose_tier),
        "service_running": service_running,
        "capturing": capturing,
        "state": state,
        "session_id": session_id,
        "camera_count": camera_count,
        "keyframes": keyframes,
        "ingest_rate_hz": ingest_rate_hz,
        "compute_node_id": compute_node_id,
    }))
    .into_response()
}

/// The `PUT /api/atlas/config` body. Every field is optional (patch semantics):
/// only the provided keys are written, so a toggle of `enabled` alone never wipes
/// the cameras or the profile. Every field maps to one the capture service reads.
#[derive(Debug, Deserialize)]
pub struct AtlasConfigBody {
    /// Turn capture on/off for this drone (the per-drone enable).
    pub enabled: Option<bool>,
    /// `orbit` / `lawnmower` / `freeform` / `inspection`.
    pub capture_profile: Option<String>,
    /// The camera set (1..N), each `{id, role, enabled, reconstruct}`. Written
    /// verbatim when supplied; absent leaves the existing cameras untouched.
    pub cameras: Option<Value>,
}

/// `PUT /api/atlas/config` → surgically write the `atlas:` block + restart the
/// capture service.
/// Validate the patch body before any write. Both the capture profile and every
/// camera role must be a known enum value — an out-of-set value would fail the
/// whole `atlas:` block's deserialize and silently default it to disabled (a
/// status surface that lies), so it is caught here at the API edge. `Err` carries
/// the 400 detail message; an absent/empty field is a no-op.
fn validate_atlas_config_body(body: &AtlasConfigBody) -> Result<(), String> {
    if let Some(profile) = body.capture_profile.as_deref() {
        if !profile.is_empty() && !VALID_CAPTURE_PROFILES.contains(&profile) {
            return Err(format!(
                "invalid capture_profile `{profile}` (expected one of {})",
                VALID_CAPTURE_PROFILES.join(" / ")
            ));
        }
    }
    if let Some(cameras) = body.cameras.as_ref().and_then(|c| c.as_array()) {
        for cam in cameras {
            if let Some(role) = cam.get("role") {
                let ok = role
                    .as_str()
                    .is_some_and(|r| VALID_CAMERA_ROLES.contains(&r));
                if !ok {
                    return Err(format!(
                        "invalid camera role {role} (expected one of {})",
                        VALID_CAMERA_ROLES.join(" / ")
                    ));
                }
            }
        }
    }
    Ok(())
}

pub async fn put_atlas_config(Json(body): Json<AtlasConfigBody>) -> Response {
    if let Err(msg) = validate_atlas_config_body(&body) {
        return detail(StatusCode::BAD_REQUEST, msg);
    }
    let effective_enabled = match write_atlas_block(&config_yaml_path(), &body) {
        Ok(enabled) => enabled,
        Err(msg) => return detail(StatusCode::INTERNAL_SERVER_ERROR, msg),
    };
    let restart = restart_unit(ATLAS_UNIT);
    Json(json!({
        "status": "ok",
        "enabled": effective_enabled,
        "restart": restart,
    }))
    .into_response()
}

/// Surgically write the provided `atlas.*` fields, preserving every other key +
/// section. An absent file starts from an empty mapping; a non-mapping `atlas`
/// node is replaced with an empty mapping. Returns the effective `enabled` after
/// the write for the response.
fn write_atlas_block(config_path: &Path, body: &AtlasConfigBody) -> Result<bool, String> {
    use serde_norway::{Mapping, Value as Yaml};

    let mut data: Yaml = match std::fs::read_to_string(config_path) {
        Ok(text) => match serde_norway::from_str::<Yaml>(&text) {
            Ok(v) if v.is_mapping() => v,
            _ => Yaml::Mapping(Mapping::new()),
        },
        Err(_) => Yaml::Mapping(Mapping::new()),
    };

    let effective_enabled;
    {
        let root = data
            .as_mapping_mut()
            .ok_or_else(|| "config root is not a mapping".to_string())?;
        // Get-or-create the top-level `atlas:` mapping (replace a non-mapping).
        if !root.get("atlas").map(|v| v.is_mapping()).unwrap_or(false) {
            root.insert(
                Yaml::String("atlas".to_string()),
                Yaml::Mapping(Mapping::new()),
            );
        }
        let atlas = root
            .get_mut("atlas")
            .and_then(|v| v.as_mapping_mut())
            .ok_or_else(|| "atlas section is not a mapping".to_string())?;

        if let Some(enabled) = body.enabled {
            atlas.insert(Yaml::String("enabled".into()), Yaml::Bool(enabled));
        }
        if let Some(profile) = body.capture_profile.as_deref().filter(|s| !s.is_empty()) {
            atlas.insert(
                Yaml::String("capture_profile".into()),
                Yaml::String(profile.to_string()),
            );
        }
        if let Some(cameras) = &body.cameras {
            let yaml_cams = serde_norway::to_value(cameras).map_err(|e| e.to_string())?;
            atlas.insert(Yaml::String("cameras".into()), yaml_cams);
        }

        effective_enabled = atlas
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
    }

    let out = serde_norway::to_string(&data).map_err(|e| e.to_string())?;
    write_atomic(config_path, out.as_bytes())?;
    Ok(effective_enabled)
}

/// Forward a capture command to the control socket and shape the reply. An
/// unreachable / non-replying socket (the service not running) is a 503 so the
/// action is never silently dropped, matching the plugin-config write posture.
async fn forward_capture(result: Result<CaptureStatus, AtlasControlError>) -> Response {
    match result {
        Ok(status) => Json(status).into_response(),
        Err(e) => detail(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("atlas capture service unavailable: {e}"),
        ),
    }
}

/// `POST /api/atlas/capture/start`.
pub async fn post_capture_start() -> Response {
    forward_capture(AtlasControlClient::default_socket().start().await).await
}

/// `POST /api/atlas/capture/stop` — finalize + bag → triggers reconstruction.
pub async fn post_capture_stop() -> Response {
    forward_capture(AtlasControlClient::default_socket().stop().await).await
}

/// `POST /api/atlas/capture/pause`.
pub async fn post_capture_pause() -> Response {
    forward_capture(AtlasControlClient::default_socket().pause().await).await
}

/// `POST /api/atlas/capture/resume`.
pub async fn post_capture_resume() -> Response {
    forward_capture(AtlasControlClient::default_socket().resume().await).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_yaml(path: &Path) -> serde_norway::Value {
        serde_norway::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn write_enables_atlas_preserving_other_sections() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(&cfg, "agent:\n  name: my-drone\nvision:\n  enabled: true\n").unwrap();

        let body = AtlasConfigBody {
            enabled: Some(true),
            capture_profile: Some("orbit".into()),
            cameras: None,
        };
        let effective = write_atlas_block(&cfg, &body).unwrap();
        assert!(effective);

        let parsed = read_yaml(&cfg);
        let atlas = parsed.get("atlas").expect("atlas written");
        assert_eq!(atlas.get("enabled").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(
            atlas.get("capture_profile").and_then(|v| v.as_str()),
            Some("orbit")
        );
        // Unrelated sections survive.
        assert_eq!(
            parsed
                .get("agent")
                .and_then(|a| a.get("name"))
                .and_then(|n| n.as_str()),
            Some("my-drone")
        );
        assert_eq!(
            parsed
                .get("vision")
                .and_then(|v| v.get("enabled"))
                .and_then(|e| e.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn patch_semantics_only_touch_provided_keys() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(
            &cfg,
            "atlas:\n  enabled: true\n  capture_profile: orbit\n  cameras:\n    - id: uvc-0\n      enabled: true\n",
        )
        .unwrap();

        // Disable only; the profile + cameras must survive untouched.
        let body = AtlasConfigBody {
            enabled: Some(false),
            capture_profile: None,
            cameras: None,
        };
        let effective = write_atlas_block(&cfg, &body).unwrap();
        assert!(!effective);

        let parsed = read_yaml(&cfg);
        let atlas = parsed.get("atlas").unwrap();
        assert_eq!(atlas.get("enabled").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(
            atlas.get("capture_profile").and_then(|v| v.as_str()),
            Some("orbit")
        );
        assert!(atlas.get("cameras").and_then(|c| c.as_sequence()).is_some());
    }

    #[test]
    fn cameras_are_written_verbatim_when_supplied() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(&cfg, "atlas:\n  enabled: false\n").unwrap();

        let body = AtlasConfigBody {
            enabled: Some(true),
            capture_profile: None,
            cameras: Some(json!([
                {"id": "front", "role": "primary", "enabled": true, "reconstruct": true},
                {"id": "down", "role": "down", "enabled": false, "reconstruct": false}
            ])),
        };
        write_atlas_block(&cfg, &body).unwrap();

        let view = read_atlas_config_view(&cfg);
        assert!(view.enabled);
        // Two cameras written; one enabled.
        assert_eq!(view.cameras_configured, 1);
    }

    #[test]
    fn config_view_defaults_when_no_atlas_block() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(&cfg, "agent:\n  profile: drone\n").unwrap();
        let view = read_atlas_config_view(&cfg);
        assert!(!view.enabled);
        assert_eq!(view.cameras_configured, 0);
        assert_eq!(view.capture_profile, "freeform");
        assert_eq!(view.profile, "drone");
    }

    #[test]
    fn config_view_missing_file_is_all_defaults() {
        let view = read_atlas_config_view(Path::new("/nonexistent/ados/config.yaml"));
        assert!(!view.enabled);
        assert_eq!(view.cameras_configured, 0);
        assert_eq!(view.profile, "drone");
    }

    #[tokio::test]
    async fn put_config_rejects_an_out_of_enum_capture_profile() {
        // "balanced" is not in the capture-profile enum → 400, and no config
        // write / unit restart happens (validation returns before both).
        let resp = put_atlas_config(Json(AtlasConfigBody {
            enabled: Some(true),
            capture_profile: Some("balanced".into()),
            cameras: None,
        }))
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn put_config_rejects_an_out_of_enum_camera_role() {
        // "navigation" is not a CameraRole → 400 before any config write / restart.
        // An unknown role would fail the whole atlas block's parse and silently
        // default it to disabled, so it is caught at the API edge.
        let resp = put_atlas_config(Json(AtlasConfigBody {
            enabled: Some(true),
            capture_profile: None,
            cameras: Some(json!([{"id": "front", "role": "navigation", "enabled": true}])),
        }))
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn validate_body_accepts_known_roles_and_rejects_unknown_or_non_string() {
        // Every valid role passes.
        let ok = AtlasConfigBody {
            enabled: Some(true),
            capture_profile: Some("orbit".into()),
            cameras: Some(json!([{"id": "a", "role": "primary"}, {"id": "b", "role": "down"}])),
        };
        assert!(validate_atlas_config_body(&ok).is_ok());

        // An unknown role is rejected.
        let bad = AtlasConfigBody {
            enabled: None,
            capture_profile: None,
            cameras: Some(json!([{"id": "a", "role": "navigation"}])),
        };
        assert!(validate_atlas_config_body(&bad).is_err());

        // A non-string role is rejected, not written verbatim.
        let non_string = AtlasConfigBody {
            enabled: None,
            capture_profile: None,
            cameras: Some(json!([{"id": "a", "role": 7}])),
        };
        assert!(validate_atlas_config_body(&non_string).is_err());

        // No role key at all is left to the schema (a partial patch).
        let no_role = AtlasConfigBody {
            enabled: None,
            capture_profile: None,
            cameras: Some(json!([{"id": "a", "enabled": true}])),
        };
        assert!(validate_atlas_config_body(&no_role).is_ok());
    }

    async fn readiness_body(resp: Response) -> Value {
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn readiness_reports_idle_when_the_service_is_down() {
        // A `None` live status is the "control socket unreachable → service not
        // running" case; the route must degrade to a not-running / idle reading,
        // never fail. The config path, the live status, and the forwarder sidecar
        // path are threaded in explicitly (no env mutation, no live socket), so the
        // reading is deterministic and this test cannot race any other test.
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(
            &cfg,
            "atlas:\n  enabled: true\n  cameras:\n    - id: front\n      enabled: true\n",
        )
        .unwrap();

        // No forwarder handoff on disk → compute_node_id is null.
        let resp = build_atlas_readiness(&cfg, None, &dir.path().join("atlas-forward.json"));
        let body = readiness_body(resp).await;
        assert_eq!(body["enabled"], json!(true));
        assert_eq!(body["cameras_configured"], json!(1));
        assert_eq!(body["service_running"], json!(false));
        assert_eq!(body["capturing"], json!(false));
        assert_eq!(body["state"], json!("idle"));
        assert!(body["session_id"].is_null());
        assert!(body["compute_node_id"].is_null());
    }

    #[tokio::test]
    async fn readiness_surfaces_the_forwarders_compute_node() {
        // A fresh forwarder handoff carrying a compute node → readiness surfaces it,
        // so the GCS shows the node the stream is going to instead of null (the bug:
        // readiness read only the capture status, which never carries this).
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(&cfg, "atlas:\n  enabled: true\n").unwrap();

        let fwd_path = dir.path().join("atlas-forward.json");
        let forward = AtlasForwardStatus {
            version: ATLAS_FORWARD_SIDECAR_VERSION,
            compute_node_id: Some("workstation-01".into()),
            bearer: Some("direct-lan".into()),
            last_kf_at_ms: Some(1_700),
            generated_at_ms: 1_699,
        };
        std::fs::write(&fwd_path, serde_json::to_vec(&forward).unwrap()).unwrap();

        let resp = build_atlas_readiness(&cfg, None, &fwd_path);
        let body = readiness_body(resp).await;
        assert_eq!(body["compute_node_id"], json!("workstation-01"));
    }

    #[test]
    fn a_stale_forward_handoff_is_dropped_so_no_dead_node_lingers() {
        // A handoff older than the freshness window is treated as absent (rule 44):
        // a dead forwarder must not keep reporting a compute node that is gone.
        let dir = tempfile::tempdir().unwrap();
        let fwd_path = dir.path().join("atlas-forward.json");
        let forward = AtlasForwardStatus {
            version: ATLAS_FORWARD_SIDECAR_VERSION,
            compute_node_id: Some("workstation-01".into()),
            bearer: Some("wfb-relay".into()),
            last_kf_at_ms: Some(9),
            generated_at_ms: 9,
        };
        std::fs::write(&fwd_path, serde_json::to_vec(&forward).unwrap()).unwrap();

        // Fresh now → read back.
        let fresh = read_fresh_forward_status(&fwd_path, SystemTime::now());
        assert_eq!(
            fresh.and_then(|f| f.compute_node_id).as_deref(),
            Some("workstation-01")
        );

        // A "now" far past the write time makes the file older than the window →
        // dropped, so readiness would surface null.
        let future = SystemTime::now() + Duration::from_secs(3600);
        assert!(read_fresh_forward_status(&fwd_path, future).is_none());

        // A missing handoff file → None (no forwarder has run yet).
        assert!(
            read_fresh_forward_status(&dir.path().join("nope.json"), SystemTime::now()).is_none()
        );
    }
}
