//! The uplink-priority and AP-config write routes.

use super::*;

/// The `PUT .../network/priority` request body: the ordered uplink list. There is no
/// schema pre-validation, so an empty (or non-string) list reaches the handler and is
/// rejected by the guard below with a 400. The valid path is a non-empty list of
/// strings.
#[derive(Debug, Deserialize)]
pub struct UplinkPriorityUpdate {
    pub priority: Vec<Value>,
}

/// `PUT .../network/priority` → `{"priority": [...]}`.
///
/// Gates on the ground-station profile (404 on a drone), validates the requested
/// order (a non-empty list of distinct uplinks the router manages, else 400
/// `E_UPLINK_PRIORITY_INVALID`), atomically persists `{"priority": [...]}` to the
/// uplink file, and echoes the persisted list. The `ados-net` daemon reads the
/// same file, so the persist is the whole effect. A file-write failure degrades
/// to the FastAPI 500 `E_UPLINK_PRIORITY_FAILED` rather than panicking.
pub async fn put_network_priority(
    State(_state): State<AppState>,
    Json(update): Json<UplinkPriorityUpdate>,
) -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }

    let strings = match validate_priority(&update.priority) {
        Ok(s) => s,
        Err(msg) => {
            return error_body(StatusCode::BAD_REQUEST, "E_UPLINK_PRIORITY_INVALID", &msg);
        }
    };

    if let Err(msg) = save_priority(&gs_uplink_json(), &strings) {
        return error_body(
            StatusCode::INTERNAL_SERVER_ERROR,
            "E_UPLINK_PRIORITY_FAILED",
            &msg,
        );
    }

    Json(json!({ "priority": strings })).into_response()
}

/// Validate the requested priority list, returning the list of strings on
/// success. Each entry must be an uplink the router manages (its default chain
/// names every one) and may appear once.
pub(super) fn validate_priority(priority: &[Value]) -> Result<Vec<String>, String> {
    use ados_net::router::failover::DEFAULT_PRIORITY;
    if priority.is_empty() {
        return Err("priority must be a non-empty list of strings".to_string());
    }
    let mut out: Vec<String> = Vec::with_capacity(priority.len());
    for entry in priority {
        let Some(name) = entry.as_str() else {
            return Err("priority must be a non-empty list of strings".to_string());
        };
        if !DEFAULT_PRIORITY.contains(&name) {
            return Err(format!(
                "unknown uplink {name:?}; expected one of {}",
                DEFAULT_PRIORITY.join(", ")
            ));
        }
        if out.iter().any(|seen| seen == name) {
            return Err(format!("uplink {name:?} is listed twice"));
        }
        out.push(name.to_string());
    }
    Ok(out)
}

/// Atomically and durably persist the priority list to `path` as
/// `{"priority": [...]}`: write a tmp sibling unique to this write, fsync it,
/// then rename it over the target, so concurrent PUTs never share or rename
/// each other's half-written file.
pub(super) fn save_priority(path: &Path, priority: &[String]) -> Result<(), String> {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    static WRITE_SEQ: AtomicU64 = AtomicU64::new(0);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        WRITE_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = path.with_file_name(tmp_name);
    let body = json!({ "priority": priority }).to_string();
    let written = std::fs::File::create(&tmp)
        .and_then(|mut f| f.write_all(body.as_bytes()).and_then(|()| f.sync_all()))
        .and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.to_string());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// PUT /api/v1/ground-station/network/ap — apply AP config + start/stop.
// ---------------------------------------------------------------------------

/// The `PUT .../network/ap` request body.
#[derive(Debug, Default, Deserialize)]
pub struct ApUpdate {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub ssid: Option<String>,
    #[serde(default)]
    pub passphrase: Option<String>,
    #[serde(default)]
    pub channel: Option<i64>,
}

/// `PUT .../network/ap` → the `_ap_view` body.
///
/// Gates on the ground-station profile (404 on a drone), refuses an SSID,
/// passphrase or channel hostapd cannot take with 400 `E_AP_INVALID_CONFIG`
/// (a newline would inject a hostapd directive; a short passphrase or a bad
/// channel would stop the AP on restart), forwards an `ap_config` op to the
/// `ados-net` command socket (the daemon applies it through its live hostapd
/// manager, honours the start/stop `enabled` hint, and replies with the
/// `_ap_view` body), persists the channel/ssid into the agent config for reboot
/// survival (best-effort, matching the FastAPI `_save_config`), and returns the
/// view. A failed apply maps to the FastAPI 500 `E_AP_APPLY_FAILED`; an
/// unreachable socket → 503.
pub async fn put_network_ap(
    State(_state): State<AppState>,
    Json(update): Json<ApUpdate>,
) -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }

    if let Err(msg) = validate_ap_update(&update) {
        return error_body(StatusCode::BAD_REQUEST, "E_AP_INVALID_CONFIG", &msg);
    }

    let request = json!({
        "op": "ap_config",
        "ssid": update.ssid,
        "passphrase": update.passphrase,
        "channel": update.channel,
        "enabled": update.enabled,
    });
    let view = match net_cmd(&request).await {
        NetCmd::Reply(r) => r,
        NetCmd::Error(msg) => {
            return error_body(StatusCode::INTERNAL_SERVER_ERROR, "E_AP_APPLY_FAILED", &msg)
        }
        NetCmd::Unavailable | NetCmd::TimedOut => return socket_unavailable("E_AP_APPLY_FAILED"),
    };

    // Persist channel / SSID back to the agent config for reboot survival (only
    // when a value was supplied). The AP already carries the change, so a persist
    // failure does not fail the request, but the body says whether it will
    // survive a restart.
    let mut view = view;
    if update.channel.is_some() || update.ssid.is_some() {
        match persist_hotspot(&config_yaml_path(), update.channel, update.ssid.as_deref()) {
            Ok(()) => {
                view.insert("persisted".to_string(), json!(true));
            }
            Err(e) => {
                tracing::error!(error = %e, "hotspot settings applied but not persisted");
                view.insert("persisted".to_string(), json!(false));
                view.insert("persist_error".to_string(), json!(e));
            }
        }
    }

    Json(Value::Object(view)).into_response()
}

/// Merge the supplied `network.hotspot.channel` / `ssid` into the agent config
/// through the shared config store, preserving every other key.
pub(super) fn persist_hotspot(
    config_path: &Path,
    channel: Option<i64>,
    ssid: Option<&str>,
) -> Result<(), String> {
    use serde_norway::Value as Yaml;
    update_config(config_path, |root| {
        let hotspot = section_path(root, &["network", "hotspot"]);
        if let Some(c) = channel {
            hotspot.insert(Yaml::String("channel".to_string()), Yaml::Number(c.into()));
        }
        if let Some(s) = ssid {
            hotspot.insert(
                Yaml::String("ssid".to_string()),
                Yaml::String(s.to_string()),
            );
        }
        Ok(())
    })
    .map(|_| ())
    .map_err(|e| e.to_string())
}
