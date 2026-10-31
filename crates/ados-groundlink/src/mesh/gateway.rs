//! batman-adv gateway mode: the decision from role + `cloud_uplink` + the live
//! uplink flag, its `batctl gw_mode` application, the operator's persisted
//! gateway pin, and loading the kernel module.

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use super::batctl;

/// Bandwidth hint advertised with `gw_mode server` (10 Mbit down, 2 up).
pub const GATEWAY_BANDWIDTH_DEFAULT: &str = "10000/2000";
const CMD_TIMEOUT: Duration = Duration::from_secs(5);

/// The gateway mode for this node. `force_on` advertises, `force_off` does not,
/// `auto` advertises iff the uplink is live. A non-advertising receiver runs as
/// a gateway client; every other non-advertising node runs with it off.
pub fn gateway_mode_for(role: &str, cloud_uplink: &str, has_uplink: bool) -> &'static str {
    let advertise = match cloud_uplink {
        "force_on" => true,
        "force_off" => false,
        _ => has_uplink,
    };
    if advertise {
        "server"
    } else if role == "receiver" {
        "client"
    } else {
        "off"
    }
}

/// The `batctl` argv for `mode`.
pub fn gw_mode_args(mode: &str) -> Vec<&str> {
    let mut args = vec!["gw_mode", mode];
    if mode == "server" {
        args.push(GATEWAY_BANDWIDTH_DEFAULT);
    }
    args
}

/// Apply `mode`; `false` (logged) when `batctl` refused it.
pub async fn apply_gateway_mode(mode: &str) -> bool {
    let (rc, _, err) = batctl::run("batctl", &gw_mode_args(mode), CMD_TIMEOUT).await;
    if rc != 0 {
        tracing::warn!(mode, err = %err.trim(), "gw_mode_apply_failed");
        return false;
    }
    true
}

/// The pinned gateway MAC in the operator's preference file
/// (`{"mode": "pinned", "pinned_mac": "..."}`), `None` for any other mode or an
/// unreadable file.
pub fn pinned_gateway(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let data: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "gateway_preference_read_failed");
            return None;
        }
    };
    if data.get("mode").and_then(Value::as_str) != Some("pinned") {
        return None;
    }
    match data.get("pinned_mac") {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Null) | None => None,
        Some(other) => Some(other.to_string()),
    }
}

/// Re-apply the operator's gateway pin so it survives restarts. Returns the
/// pinned MAC when applied.
pub async fn apply_persisted_gateway_preference(path: &Path) -> Option<String> {
    let mac = pinned_gateway(path)?;
    let (rc, _, err) = batctl::run("batctl", &["gw_sel", &mac], CMD_TIMEOUT).await;
    if rc != 0 {
        tracing::warn!(mac = %mac, err = %err.trim(), "gateway_pin_apply_failed");
        return None;
    }
    tracing::info!(mac = %mac, "gateway_pin_applied");
    Some(mac)
}

/// Load the batman-adv module.
pub async fn modprobe_batman() -> bool {
    let (rc, _, err) = batctl::run("modprobe", &["batman-adv"], Duration::from_secs(10)).await;
    if rc != 0 {
        tracing::error!(err = %err.trim(), "modprobe_batman_failed");
        return false;
    }
    true
}
