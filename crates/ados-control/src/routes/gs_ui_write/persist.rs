//! Config persistence: the YAML `ground_station.ui.<section>` and `ground_station.kiosk` writers.

use super::*;

/// Merge `value` into `ground_station.ui.<ui_section>` of the on-disk YAML config
/// through the shared config store, preserving every other key and the mapping
/// insertion order. Returns `Err(message)` on any read/parse/serialize/write
/// fault so the caller can map it to the `E_UI_SAVE_FAILED` 500 — including a
/// document the store refuses to write over and the EPERM a non-root front gets
/// on the 0600 root-owned config.
pub(super) fn persist_gs_ui_section(
    config_path: &Path,
    ui_section: &str,
    value: &Value,
) -> Result<(), String> {
    use serde_norway::Value as Yaml;
    update_config(config_path, |root| {
        section_path(root, &["ground_station", "ui"])
            .insert(Yaml::String(ui_section.to_string()), json_to_yaml(value));
        Ok(())
    })
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// Convert a `serde_json::Value` into a `serde_norway::Value`, preserving the
/// scalar / array / object shape. Used to nest a UI section under the YAML config
/// tree without going through a string round-trip.
pub(super) fn json_to_yaml(value: &Value) -> serde_norway::Value {
    use serde_norway::{Mapping, Value as Yaml};
    match value {
        Value::Null => Yaml::Null,
        Value::Bool(b) => Yaml::Bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Yaml::Number(i.into())
            } else if let Some(u) = n.as_u64() {
                Yaml::Number(u.into())
            } else if let Some(f) = n.as_f64() {
                Yaml::Number(f.into())
            } else {
                Yaml::Null
            }
        }
        Value::String(s) => Yaml::String(s.clone()),
        Value::Array(arr) => Yaml::Sequence(arr.iter().map(json_to_yaml).collect()),
        Value::Object(map) => {
            let mut m = Mapping::new();
            for (k, v) in map {
                m.insert(Yaml::String(k.clone()), json_to_yaml(v));
            }
            Yaml::Mapping(m)
        }
    }
}

// ---------------------------------------------------------------------------
// Kiosk config persist: merge fields into ground_station.kiosk (the single source
// of truth the kiosk service + PUT /api/config also use).
// ---------------------------------------------------------------------------

/// Merge the supplied kiosk fields into `ground_station.kiosk` of the on-disk YAML
/// config through the shared config store, preserving every other key (e.g. an
/// operator-set `minimal_layer`) and the mapping insertion order. Returns
/// `Err(message)` on any fault so the caller maps it to the `E_UI_SAVE_FAILED`
/// 500.
pub(super) fn persist_gs_kiosk_fields(
    config_path: &Path,
    fields: &[(&str, Value)],
) -> Result<(), String> {
    use serde_norway::Value as Yaml;
    update_config(config_path, |root| {
        let kiosk = section_path(root, &["ground_station", "kiosk"]);
        for (k, v) in fields {
            kiosk.insert(Yaml::String((*k).to_string()), json_to_yaml(v));
        }
        Ok(())
    })
    .map(|_| ())
    .map_err(|e| e.to_string())
}
