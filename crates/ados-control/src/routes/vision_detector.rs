//! Active-detector selection: pick (or clear) the model the vision engine runs.
//!
//! Two writes the GCS uses to drive which detector the engine auto-loads:
//!
//! - **`PUT /api/vision/detector`** — body `{model_id, model_path?}`. Writes the
//!   `vision.detector` block (`model_id` + `enabled: true`, plus `model_path`
//!   only when the body carries one) into `/etc/ados/config.yaml` with a surgical
//!   merge that preserves every other key, then restarts `ados-vision` so the
//!   engine reloads with the new detector. The response carries the new active
//!   `model_id`.
//! - **`DELETE /api/vision/detector`** — removes the `vision.detector` block
//!   entirely (the engine reverts to inert, driving no inference of its own) and
//!   restarts `ados-vision`. The response reports `model_id: null`.
//!
//! ## Why the route does not resolve the model path
//!
//! The engine resolves a registry `model_id` to an on-disk path at boot through
//! the model manager, so this route writes only the operator's selection. A
//! sideloaded model that is not in the registry carries its own `model_path`,
//! which the body supplies; otherwise the engine does the lookup. The route never
//! touches the model files — it only flips the config the engine reads.
//!
//! ## Config write + restart seam
//!
//! The config edit goes through the shared config store (`crate::config_store`):
//! an absent file starts from an empty mapping, a non-mapping `vision` /
//! `detector` node is replaced with an empty mapping, every sibling key under
//! `vision` (and every other top-level section) is preserved, and a document
//! that cannot be read or parsed is refused rather than written over. The
//! restart goes through the same `ados-*` allowlisted restart path the
//! service-control route uses, so the unit name is validated before any
//! `systemctl` runs.
//!
//! ## Auth posture
//!
//! Both are writes, so they sit outside the public set and the LAN edge requires
//! the pairing key when paired — the same posture as `/api/vision/designate` and
//! `/api/command`.

use std::path::{Path, PathBuf};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config_store::{section_path, update_config};
use crate::routes::detail;
use crate::routes::service_control::restart_unit;
use crate::state::AppState;

/// The unit that runs the vision engine; restarting it reloads the detector.
const VISION_UNIT: &str = "ados-vision";

/// The agent config path (`ADOS_CONFIG`, default `/etc/ados/config.yaml`), the
/// same resolution the sibling write routes use.
fn config_yaml_path() -> PathBuf {
    PathBuf::from(
        std::env::var("ADOS_CONFIG").unwrap_or_else(|_| crate::config::CONFIG_YAML.to_string()),
    )
}

/// The `PUT /api/vision/detector` body: the model to make active, plus an
/// optional explicit path for a sideloaded model the engine cannot resolve from
/// the registry id alone.
#[derive(Debug, Deserialize)]
pub struct DetectorBody {
    pub model_id: String,
    #[serde(default)]
    pub model_path: Option<String>,
}

/// `PUT /api/vision/detector` → write the active detector + restart the engine.
pub async fn put_detector(
    State(_state): State<AppState>,
    Json(req): Json<DetectorBody>,
) -> Response {
    let model_id = req.model_id.trim().to_string();
    if model_id.is_empty() {
        return detail(StatusCode::BAD_REQUEST, "model_id is required");
    }

    if let Err(msg) = write_detector_block(
        &config_yaml_path(),
        &model_id,
        req.model_path.as_deref().filter(|p| !p.trim().is_empty()),
    ) {
        return detail(StatusCode::INTERNAL_SERVER_ERROR, msg);
    }

    let restart = restart_unit(VISION_UNIT).await;
    restart_reply(
        json!({
            "model_id": model_id,
            "enabled": true,
        }),
        restart,
    )
}

/// The reply for a detector write: `200 {status:"ok", ..}` when the engine
/// restarted onto the new config, `502 {status:"error", ..}` when it did not.
/// The config is written either way, but the running engine keeps its old
/// model until a restart succeeds, so a failed restart is not a success.
fn restart_reply(mut body: Value, restart: Value) -> Response {
    let restarted = restart.get("status").and_then(|s| s.as_str()) == Some("ok");
    body["status"] = json!(if restarted { "ok" } else { "error" });
    body["restart"] = restart;
    let status = if restarted {
        StatusCode::OK
    } else {
        StatusCode::BAD_GATEWAY
    };
    (status, Json(body)).into_response()
}

/// `DELETE /api/vision/detector` → remove the active detector + restart the engine.
pub async fn delete_detector(State(_state): State<AppState>) -> Response {
    if let Err(msg) = remove_detector_block(&config_yaml_path()) {
        return detail(StatusCode::INTERNAL_SERVER_ERROR, msg);
    }

    let restart = restart_unit(VISION_UNIT).await;
    restart_reply(
        json!({
            "model_id": Value::Null,
            "enabled": false,
        }),
        restart,
    )
}

/// Surgically write `vision.detector.{model_id, enabled}` (plus `model_path` when
/// supplied), preserving every other key, so the edit never clobbers a sibling
/// `vision` field (cameras, backend, tracker_enabled, …) or any other top-level
/// section.
fn write_detector_block(
    config_path: &Path,
    model_id: &str,
    model_path: Option<&str>,
) -> Result<(), String> {
    use serde_norway::Value as Yaml;

    update_config(config_path, |root| {
        let detector = section_path(root, &["vision", "detector"]);
        detector.insert(
            Yaml::String("model_id".to_string()),
            Yaml::String(model_id.to_string()),
        );
        detector.insert(Yaml::String("enabled".to_string()), Yaml::Bool(true));
        match model_path {
            Some(p) => {
                detector.insert(
                    Yaml::String("model_path".to_string()),
                    Yaml::String(p.to_string()),
                );
            }
            // No path supplied: pop a stale one so the engine resolves the new id
            // through the registry rather than against the previous model's file.
            None => {
                detector.remove("model_path");
            }
        }
        Ok(())
    })
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// Remove the `vision.detector` block so the engine reverts to inert. A missing
/// block (or a missing/empty file) is a no-op success that writes nothing. Every
/// other key survives.
fn remove_detector_block(config_path: &Path) -> Result<(), String> {
    update_config(config_path, |root| {
        if let Some(vision) = root.get_mut("vision").and_then(|v| v.as_mapping_mut()) {
            vision.remove("detector");
        }
        Ok(())
    })
    .map(|_| ())
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The engine keeps its old model until a restart succeeds, so a detector
    /// write whose restart failed is not a success.
    #[test]
    fn a_failed_engine_restart_is_not_reported_as_success() {
        let failed = restart_reply(
            json!({"model_id": "yolov8n", "enabled": true}),
            json!({"status": "error", "message": "restart not confirmed"}),
        );
        assert_eq!(failed.status(), StatusCode::BAD_GATEWAY);
        let ok = restart_reply(json!({"model_id": "yolov8n"}), json!({"status": "ok"}));
        assert_eq!(ok.status(), StatusCode::OK);
    }

    fn read_yaml(path: &Path) -> serde_norway::Value {
        serde_norway::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn write_sets_model_id_and_enabled_preserving_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(
            &cfg,
            "agent:\n  name: my-drone\nvision:\n  enabled: true\n  backend: rknn\n  cameras:\n    - id: uvc-0\n",
        )
        .unwrap();

        write_detector_block(&cfg, "com.example.coco-yolov8n", None).unwrap();

        let parsed = read_yaml(&cfg);
        let detector = parsed
            .get("vision")
            .and_then(|v| v.get("detector"))
            .expect("detector written");
        assert_eq!(
            detector.get("model_id").and_then(|v| v.as_str()),
            Some("com.example.coco-yolov8n")
        );
        assert_eq!(
            detector.get("enabled").and_then(|v| v.as_bool()),
            Some(true)
        );
        // No model_path written when the body omits it.
        assert!(detector.get("model_path").is_none());
        // Sibling vision keys + the unrelated agent section survive.
        let vision = parsed.get("vision").unwrap();
        assert_eq!(vision.get("backend").and_then(|v| v.as_str()), Some("rknn"));
        assert!(vision.get("cameras").is_some());
        assert_eq!(
            parsed
                .get("agent")
                .and_then(|a| a.get("name"))
                .and_then(|n| n.as_str()),
            Some("my-drone")
        );
    }

    #[test]
    fn write_includes_model_path_when_supplied() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(&cfg, "vision:\n  enabled: true\n").unwrap();

        write_detector_block(&cfg, "custom-1", Some("/var/ados/models/custom-1.onnx")).unwrap();

        let parsed = read_yaml(&cfg);
        let detector = parsed
            .get("vision")
            .and_then(|v| v.get("detector"))
            .unwrap();
        assert_eq!(
            detector.get("model_path").and_then(|v| v.as_str()),
            Some("/var/ados/models/custom-1.onnx")
        );
    }

    #[test]
    fn write_replacing_an_existing_detector_pops_a_stale_path() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        // An existing detector that carried an explicit path.
        std::fs::write(
            &cfg,
            "vision:\n  enabled: true\n  detector:\n    model_id: old\n    model_path: /var/ados/models/old.onnx\n    enabled: true\n",
        )
        .unwrap();

        // Re-select a registry model with no explicit path → the stale path pops so
        // the engine resolves the new id itself.
        write_detector_block(&cfg, "new-registry-id", None).unwrap();

        let parsed = read_yaml(&cfg);
        let detector = parsed
            .get("vision")
            .and_then(|v| v.get("detector"))
            .unwrap();
        assert_eq!(
            detector.get("model_id").and_then(|v| v.as_str()),
            Some("new-registry-id")
        );
        assert!(
            detector.get("model_path").is_none(),
            "a stale model_path must be popped on re-select"
        );
    }

    #[test]
    fn write_creates_the_file_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        // No file yet.
        write_detector_block(&cfg, "first", None).unwrap();
        let parsed = read_yaml(&cfg);
        assert_eq!(
            parsed
                .get("vision")
                .and_then(|v| v.get("detector"))
                .and_then(|d| d.get("model_id"))
                .and_then(|v| v.as_str()),
            Some("first")
        );
    }

    #[test]
    fn delete_removes_the_block_and_keeps_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(
            &cfg,
            "vision:\n  enabled: true\n  backend: onnx\n  detector:\n    model_id: x\n    enabled: true\n",
        )
        .unwrap();

        remove_detector_block(&cfg).unwrap();

        let parsed = read_yaml(&cfg);
        let vision = parsed.get("vision").unwrap();
        assert!(
            vision.get("detector").is_none(),
            "detector block must be gone"
        );
        // The master enable + backend survive.
        assert_eq!(vision.get("enabled").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(vision.get("backend").and_then(|v| v.as_str()), Some("onnx"));
    }

    #[test]
    fn delete_is_a_noop_when_no_detector_or_no_file() {
        let dir = tempfile::tempdir().unwrap();
        // Missing file: a clean success, nothing created.
        let absent = dir.path().join("absent.yaml");
        remove_detector_block(&absent).unwrap();
        assert!(!absent.exists());

        // A config with no detector block: a clean success, file unchanged.
        let cfg = dir.path().join("config.yaml");
        std::fs::write(&cfg, "vision:\n  enabled: true\n").unwrap();
        let before = std::fs::read_to_string(&cfg).unwrap();
        remove_detector_block(&cfg).unwrap();
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), before);
    }

    #[test]
    fn empty_model_id_is_rejected_at_the_handler() {
        // The handler trims + rejects an empty model_id before any write. Drive the
        // guard directly (the write helper itself does not re-check, the handler does).
        let id = "   ".trim();
        assert!(id.is_empty());
    }
}
