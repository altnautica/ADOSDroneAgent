//! `GET /api/v1/storage`: the capture storage root, every mounted block
//! volume, capture usage against the budget, and the write-rate probe.
//!
//! Capture usage and admission come from the supervisor janitor's
//! `capture-storage.json`; the volume list is read live. Capture storage
//! exists on the camera-owning profiles (drone, visor) only.

use std::time::SystemTime;

use axum::extract::State;
use axum::Json;
use serde_json::{json, Map, Value};

use ados_config::capture_storage::{list_volumes_live, Volume};

use super::{fresh_map, with_present, SensingPaths};
use crate::state::AppState;

/// The profiles that record capture sessions.
pub(crate) const CAPTURE_PROFILES: &[&str] = &["drone", "visor"];

pub async fn get_storage(State(state): State<AppState>) -> Json<Value> {
    let profile = super::profile(&state);
    let paths = SensingPaths::from_state(&state);
    let volumes = tokio::task::spawn_blocking(list_volumes_live)
        .await
        .unwrap_or_default();
    Json(storage_body(&paths, &profile, &volumes, SystemTime::now()))
}

pub(crate) fn storage_body(
    paths: &SensingPaths,
    profile: &str,
    volumes: &[Volume],
    now: SystemTime,
) -> Value {
    let side = CAPTURE_PROFILES
        .contains(&profile)
        .then(|| fresh_map(&paths.sidecar("capture-storage.json"), None, now))
        .flatten();
    let Some(side) = side else {
        let mut rest = Map::new();
        rest.insert("capture_root".into(), Value::Null);
        rest.insert("volumes".into(), json!([]));
        rest.insert("capture".into(), Value::Null);
        rest.insert("write_probe".into(), Value::Null);
        return with_present(false, rest);
    };
    let capture_mount = side
        .get("volume")
        .and_then(|v| v.get("mount"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let vols: Vec<Value> = volumes
        .iter()
        .map(|v| {
            let mut o = serde_json::to_value(v).unwrap_or_else(|_| json!({}));
            o["is_capture"] = json!(v.mount == capture_mount);
            o
        })
        .collect();
    let mut rest = Map::new();
    rest.insert(
        "capture_root".into(),
        side.get("capture_root").cloned().unwrap_or(Value::Null),
    );
    rest.insert("volumes".into(), Value::Array(vols));
    rest.insert(
        "capture".into(),
        side.get("capture").cloned().unwrap_or(Value::Null),
    );
    rest.insert(
        "write_probe".into(),
        side.get("write_probe").cloned().unwrap_or(Value::Null),
    );
    with_present(true, rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIDECAR: &str = r#"{"version":1,"updated_ns":1,"capture_root":"/mnt/nvme0/ados/capture",
      "volume":{"mount":"/mnt/nvme0","device":"nvme0n1","fs":"ext4","total_bytes":1000,"free_bytes":800,"rotational":false},
      "capture":{"used_bytes":100,"budget_bytes":850,"budget_pct":85,"floor_free_bytes":5368709120,"sessions":12,
                 "uploaded":9,"evict_unuploaded":false,"state":"ok"},
      "write_probe":null}"#;

    fn vol(mount: &str, device: &str) -> Volume {
        Volume {
            mount: mount.into(),
            device: device.into(),
            fs: "ext4".into(),
            total_bytes: 1000,
            free_bytes: 800,
            rotational: false,
        }
    }

    #[test]
    fn the_body_marks_the_capture_volume_and_carries_usage() {
        let dir = tempfile::tempdir().unwrap();
        let p = SensingPaths {
            run_dir: dir.path().to_path_buf(),
            etc_dir: dir.path().to_path_buf(),
        };
        std::fs::write(p.sidecar("capture-storage.json"), SIDECAR).unwrap();
        let vols = [vol("/", "mmcblk0p2"), vol("/mnt/nvme0", "nvme0n1")];
        let body = storage_body(&p, "visor", &vols, SystemTime::now());
        let mut keys: Vec<&str> = body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "capture",
                "capture_root",
                "present",
                "volumes",
                "write_probe"
            ]
        );
        assert_eq!(body["present"], true);
        assert_eq!(body["capture_root"], "/mnt/nvme0/ados/capture");
        assert_eq!(body["volumes"][0]["is_capture"], false);
        assert_eq!(body["volumes"][1]["is_capture"], true);
        let mut vkeys: Vec<&str> = body["volumes"][1]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        vkeys.sort_unstable();
        assert_eq!(
            vkeys,
            [
                "device",
                "free_bytes",
                "fs",
                "is_capture",
                "mount",
                "rotational",
                "total_bytes"
            ]
        );
        assert_eq!(body["capture"]["state"], "ok");
        assert_eq!(body["write_probe"], Value::Null);

        // Not a capture profile: not present even with a sidecar on disk.
        let gs = storage_body(&p, "ground_station", &vols, SystemTime::now());
        assert_eq!(gs["present"], false);
        assert_eq!(gs["volumes"], json!([]));
    }

    #[test]
    fn no_janitor_report_is_not_present() {
        let dir = tempfile::tempdir().unwrap();
        let p = SensingPaths {
            run_dir: dir.path().to_path_buf(),
            etc_dir: dir.path().to_path_buf(),
        };
        let body = storage_body(&p, "drone", &[], SystemTime::now());
        assert_eq!(
            body,
            json!({"present": false, "capture_root": null, "volumes": [], "capture": null, "write_probe": null})
        );
    }
}
