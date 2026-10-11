//! FC identity: the reconciler's last verdict, verbatim from its sidecar.

use serde_json::Value;
use std::path::Path;

/// The FC identity reconciler's sidecar file name under the run directory.
pub(crate) const FC_IDENTITY_SIDECAR: &str = "fc-identity.json";

/// The FC identity reconciler's last verdict, verbatim from its sidecar
/// (`{version, enabled, outcome, wanted_system_id, current_system_id, at_ms}`),
/// or null when the file is absent or not a JSON object.
pub(crate) fn read_fc_identity(run_dir: &Path) -> Value {
    std::fs::read_to_string(run_dir.join(FC_IDENTITY_SIDECAR))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .filter(Value::is_object)
        .unwrap_or(Value::Null)
}
