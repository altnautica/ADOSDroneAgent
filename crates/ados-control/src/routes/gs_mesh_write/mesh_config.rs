//! PUT /api/v1/ground-station/mesh/config — set the configured mesh transport.

use super::*;

/// The `PUT .../mesh/config` body: three optional fields, each applied only when present.
/// [`validate_mesh_config`] holds `carrier` to `802.11s`/`ibss`, `channel` to 1..=13 and `mesh_id`
/// to a 1..=32-byte printable name before anything is written.
#[derive(Debug, Default, Deserialize)]
pub struct MeshConfigUpdate {
    #[serde(default)]
    pub mesh_id: Option<String>,
    #[serde(default)]
    pub carrier: Option<String>,
    #[serde(default)]
    pub channel: Option<i64>,
}

/// `PUT .../mesh/config` → `{mesh_id, carrier, channel, applied}`.
///
/// Gates on the profile (404 `E_PROFILE_MISMATCH`), then surgically merges the
/// supplied fields into `ground_station.mesh` of the on-disk config (preserving
/// every other key — the same approach the GS-wfb config write takes) and returns
/// the resolved view (request → existing → config-model default) plus `applied`
/// (true iff at least one field was supplied). There is no command-socket op: the
/// mesh manager reads these from the same config file on its own cadence.
pub async fn put_mesh_config(
    State(_state): State<AppState>,
    Json(update): Json<MeshConfigUpdate>,
) -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }
    put_mesh_config_at(&config_yaml_path(), &update)
}

/// The merge logic against an explicit config path (a test points it at a temp
/// file). The response mirrors the FastAPI handler, which echoes the post-mutation
/// `mesh.{mesh_id,carrier,channel}` model values + `applied`. An out-of-range
/// field is a 422 `E_INVALID_MESH_CONFIG` and nothing is written.
pub(super) fn put_mesh_config_at(config_path: &Path, update: &MeshConfigUpdate) -> Response {
    if let Err(message) = validate_mesh_config(update) {
        return nested_detail(
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({"code": "E_INVALID_MESH_CONFIG", "message": message}),
        );
    }
    match merge_mesh_config(config_path, update) {
        Ok((mesh_id, carrier, channel, applied)) => Json(json!({
            "mesh_id": mesh_id,
            "carrier": carrier,
            "channel": channel,
            "applied": applied,
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"detail": {"error": {"code": "E_CONFIG_WRITE_FAILED", "message": e}}})),
        )
            .into_response(),
    }
}

/// The mesh carriers the mesh manager can bring up.
pub(super) const MESH_CARRIERS: [&str; 2] = ["802.11s", "ibss"];

/// The 2.4 GHz channels a mesh may use.
pub(super) const MESH_CHANNELS: std::ops::RangeInclusive<i64> = 1..=13;

/// The longest mesh id 802.11s carries (the Mesh ID element, like an SSID).
pub(super) const MESH_ID_MAX_BYTES: usize = 32;

/// Check every supplied field against what the mesh manager can apply. A value
/// outside these bounds would be persisted and then fail at the next mesh
/// bring-up, far from the request that wrote it.
pub(super) fn validate_mesh_config(update: &MeshConfigUpdate) -> Result<(), String> {
    if let Some(carrier) = &update.carrier {
        if !MESH_CARRIERS.contains(&carrier.as_str()) {
            return Err(format!("carrier must be one of {MESH_CARRIERS:?}"));
        }
    }
    if let Some(channel) = update.channel {
        if !MESH_CHANNELS.contains(&channel) {
            return Err(format!(
                "channel must be between {} and {}",
                MESH_CHANNELS.start(),
                MESH_CHANNELS.end()
            ));
        }
    }
    if let Some(id) = &update.mesh_id {
        let printable = id.bytes().all(|b| b.is_ascii_graphic());
        if id.is_empty() || id.len() > MESH_ID_MAX_BYTES || !printable {
            return Err(format!(
                "mesh_id must be 1 to {MESH_ID_MAX_BYTES} printable ASCII characters with no spaces"
            ));
        }
    }
    Ok(())
}
