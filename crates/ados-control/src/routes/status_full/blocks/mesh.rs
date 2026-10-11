//! Mesh block: ground-station profile, non-direct role only.

use super::super::*;
use ados_protocol::paths;
use ados_protocol::paths::run_dir;
use ados_protocol::wfb_status::json_truthy;
use serde_json::json;
use serde_json::Map;
use serde_json::Value;
use std::path::Path;

/// Build the consolidated-status `mesh` block. Populated only when the RAW config
/// profile is exactly `"ground_station"` (the underscore form, NOT the resolved
/// wire form — matching the Python gate `app.config.agent.profile ==
/// "ground_station"`); a drone or an `"auto"`/empty config gets an empty object.
///
/// On a ground station it carries `role` + `mesh_capable`; for a `relay`/`receiver`
/// role it also folds in the `up` / `peer_count` / `selected_gateway` / `partition`
/// fields from `/run/ados/mesh-state.json`. Every read degrades to omitting that
/// field rather than failing. Mirrors the FastAPI mesh block.
pub(crate) async fn build_mesh_block(config_profile: &str) -> Value {
    if config_profile != "ground_station" {
        return json!({});
    }
    // Detected live from the attached radios; the sysfs walk runs off the runtime.
    let mesh_capable = tokio::task::spawn_blocking(ados_protocol::netif::mesh_capable)
        .await
        .unwrap_or(false);
    build_mesh_block_at(
        config_profile,
        &ados_config::mesh_role_path(),
        mesh_capable,
        &run_dir().join(paths::MESH_STATE_JSON),
    )
}

/// The path-injectable core of [`build_mesh_block`]: read the role sentinel and
/// mesh-state sidecar from explicit paths, with the live `mesh_capable` probe
/// result passed in. Pure (no env
/// reads), so a test drives it with a tempdir without touching process-global env.
pub(crate) fn build_mesh_block_at(
    config_profile: &str,
    role_path: &Path,
    mesh_capable: bool,
    mesh_state_path: &Path,
) -> Value {
    if config_profile != "ground_station" {
        return json!({});
    }

    let mut mesh = Map::new();
    let role = ados_config::read_mesh_role(role_path);
    mesh.insert("role".to_string(), json!(role));
    mesh.insert("mesh_capable".to_string(), json!(mesh_capable));

    if role == "relay" || role == "receiver" {
        if let Some(snap) = read_sidecar_object(mesh_state_path) {
            // Best-effort schema-drift signal (never reject): warn when the
            // mesh-state sidecar was written by an agent with a different schema
            // version, then read anyway. The writer const lives in the groundlink
            // crate, so compare against the shared registry.
            let got = snap.get("version").and_then(Value::as_u64).unwrap_or(0) as u16;
            if let Some(ours) = ados_protocol::contracts::sidecar_version("mesh-state") {
                ados_protocol::sidecar::check_sidecar_version("mesh-state", got, ours);
            }
            mesh.insert(
                "up".to_string(),
                json!(snap.get("up").map(json_truthy).unwrap_or(false)),
            );
            let peer_count = snap
                .get("neighbors")
                .and_then(Value::as_array)
                .map(|a| a.len())
                .unwrap_or(0);
            mesh.insert("peer_count".to_string(), json!(peer_count));
            mesh.insert(
                "selected_gateway".to_string(),
                snap.get("selected_gateway").cloned().unwrap_or(Value::Null),
            );
            mesh.insert(
                "partition".to_string(),
                json!(snap.get("partition").map(json_truthy).unwrap_or(false)),
            );
        }
    }
    Value::Object(mesh)
}
