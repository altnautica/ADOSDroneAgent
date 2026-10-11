//! PUT /api/v1/ground-station/network/share_uplink — toggle the NAT share flag.

use super::*;

/// The `PUT .../network/share_uplink` request body. A single required `enabled` flag.
#[derive(Debug, Deserialize)]
pub struct ShareUplinkUpdate {
    pub enabled: bool,
}

/// `PUT .../network/share_uplink` → `{enabled, applied, apply_error, backend}`.
///
/// Gates on the ground-station profile (404 on a drone). Persists
/// `ground_station.share_uplink` into the agent config, then asks the `ados-net`
/// daemon, which owns the sysctl + firewall, to apply it now on the active
/// uplink (the `share_uplink` command-socket op) and returns the daemon's own
/// verdict. The daemon serializes that apply against its uplink-switch
/// re-apply, so there is still one writer. A daemon that is unreachable or
/// refuses leaves the flag persisted and reported `applied:false`: it applies
/// at the daemon's next start. A persist failure maps to the FastAPI 500
/// `E_UI_SAVE_FAILED`.
pub async fn put_network_share_uplink(
    State(_state): State<AppState>,
    Json(update): Json<ShareUplinkUpdate>,
) -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }

    if let Err(msg) = persist_share_uplink(&config_yaml_path(), update.enabled) {
        return error_body(StatusCode::INTERNAL_SERVER_ERROR, "E_UI_SAVE_FAILED", &msg);
    }

    let reply = net_cmd(&json!({"op": "share_uplink", "enabled": update.enabled})).await;
    Json(share_uplink_body(update.enabled, reply)).into_response()
}

/// The share-uplink reply: the daemon's apply verdict, or `applied:false` with
/// the reason it could not be asked.
pub(super) fn share_uplink_body(enabled: bool, reply: NetCmd) -> Value {
    match reply {
        NetCmd::Reply(r) => json!({
            "enabled": enabled,
            "applied": r.get("applied").and_then(Value::as_bool).unwrap_or(false),
            "apply_error": r.get("apply_error").cloned().unwrap_or(Value::Null),
            "backend": r.get("backend").cloned().unwrap_or(Value::Null),
        }),
        NetCmd::Error(msg) => json!({
            "enabled": enabled,
            "applied": false,
            "apply_error": msg,
            "backend": Value::Null,
        }),
        NetCmd::Unavailable => json!({
            "enabled": enabled,
            "applied": false,
            "apply_error": "network daemon unreachable; the saved setting applies when it starts",
            "backend": Value::Null,
        }),
        NetCmd::TimedOut => json!({
            "enabled": enabled,
            "applied": false,
            "apply_error": "network daemon did not answer in time; the saved setting applies when it starts",
            "backend": Value::Null,
        }),
    }
}

/// Merge `ground_station.share_uplink` into the agent config through the shared
/// config store, preserving every other key. Returns the error string so the route
/// can surface `E_UI_SAVE_FAILED`.
pub(super) fn persist_share_uplink(config_path: &Path, enabled: bool) -> Result<(), String> {
    use serde_norway::Value as Yaml;
    update_config(config_path, |root| {
        section(root, "ground_station").insert(
            Yaml::String("share_uplink".to_string()),
            Yaml::Bool(enabled),
        );
        Ok(())
    })
    .map(|_| ())
    .map_err(|e| e.to_string())
}
