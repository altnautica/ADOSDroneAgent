//! Config-merge helpers.

use super::*;

/// Merge `ground_station.role` into the on-disk config through the shared config
/// store, preserving every other key.
pub(super) fn merge_ground_station_role(config_path: &Path, role: &str) -> Result<(), String> {
    use serde_norway::Value as Yaml;
    update_config(config_path, |root| {
        section(root, "ground_station").insert(
            Yaml::String("role".to_string()),
            Yaml::String(role.to_string()),
        );
        Ok(())
    })
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// Merge the supplied `ground_station.mesh` fields into the on-disk config and
/// return the resolved view values + whether anything was applied. Each view value
/// resolves over the post-mutation config: the request value when
/// supplied, else the existing on-disk value, else the config-model default
/// (`mesh_id: null`, `carrier: "802.11s"`, `channel: 1`). `applied` is true iff at
/// least one field was supplied and written. A document the config store refuses
/// to write over is an `Err`, never a silent `applied: true`.
pub(super) fn merge_mesh_config(
    config_path: &Path,
    update: &MeshConfigUpdate,
) -> Result<(Value, String, i64, bool), String> {
    use serde_norway::Value as Yaml;

    let applied = update.mesh_id.is_some() || update.carrier.is_some() || update.channel.is_some();

    let existing = update_config(config_path, |root| {
        let existing = existing_mesh(root);
        if applied {
            let mesh = section_path(root, &["ground_station", "mesh"]);
            if let Some(id) = &update.mesh_id {
                mesh.insert(
                    Yaml::String("mesh_id".to_string()),
                    Yaml::String(id.clone()),
                );
            }
            if let Some(c) = &update.carrier {
                mesh.insert(Yaml::String("carrier".to_string()), Yaml::String(c.clone()));
            }
            if let Some(ch) = update.channel {
                mesh.insert(Yaml::String("channel".to_string()), Yaml::Number(ch.into()));
            }
        }
        Ok(existing)
    })
    .map_err(|e| e.to_string())?
    .value;

    // The resolved view: request → existing → config-model default. An unset
    // request field keeps its on-disk value and an absent on-disk value reads the
    // model default.
    let mesh_id = match &update.mesh_id {
        Some(id) => Value::String(id.clone()),
        None => existing.mesh_id,
    };
    let carrier = update
        .carrier
        .clone()
        .or(existing.carrier)
        .unwrap_or_else(|| DEFAULT_CARRIER.to_string());
    let channel = update
        .channel
        .or(existing.channel)
        .unwrap_or(DEFAULT_MESH_CHANNEL);

    Ok((mesh_id, carrier, channel, applied))
}

/// The existing `ground_station.mesh` view fields already on disk. `mesh_id` is a
/// JSON value (so an absent one reads `null` like the Python `mesh_id: None`
/// default); `carrier`/`channel` are `None` when absent so the merge falls through
/// to the config-model default.
#[derive(Default)]
pub(super) struct ExistingMesh {
    pub(super) mesh_id: Value,
    pub(super) carrier: Option<String>,
    pub(super) channel: Option<i64>,
}

/// Read the existing `ground_station.mesh` view fields from the parsed config
/// mapping. An absent section reads `mesh_id: null` + no carrier/channel, so the
/// resolved view falls through to the Python `MeshConfig` defaults.
pub(super) fn existing_mesh(root: &serde_norway::Mapping) -> ExistingMesh {
    let mesh = root.get("ground_station").and_then(|v| v.get("mesh"));
    let Some(mesh) = mesh else {
        return ExistingMesh::default();
    };
    let mesh_id = match mesh.get("mesh_id").and_then(|v| v.as_str()) {
        Some(s) => Value::String(s.to_string()),
        None => Value::Null,
    };
    ExistingMesh {
        mesh_id,
        carrier: mesh
            .get("carrier")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        channel: mesh.get("channel").and_then(norway_to_i64),
    }
}

/// Coerce a serde_norway scalar to `i64`, accepting an integer or a float (the
/// Python `int(...)` over a numeric config value). `None` for a non-number.
pub(super) fn norway_to_i64(v: &serde_norway::Value) -> Option<i64> {
    match v {
        serde_norway::Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        _ => None,
    }
}
