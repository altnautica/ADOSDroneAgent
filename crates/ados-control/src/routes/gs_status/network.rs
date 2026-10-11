//! The `network` sub-block (AP-only view).

use super::*;

/// The hostapd systemd unit the live AP runs as (`_HOSTAPD_UNIT`), the
/// `systemctl is-active` source for the AP `running` state.
pub(super) const HOSTAPD_UNIT: &str = "ados-hostapd.service";

/// The AP gateway address the hostapd manager assigns (`_AP_ADDR`), reported as
/// `ap_ip` while the AP is running.
pub(super) const AP_GATEWAY_IP: &str = "192.168.4.1";

/// The AP-only network view the `/status` route carries, reproducing the Python
/// `_network_view` over the live hostapd state.
///
/// The Python `_network_view` reads the live hostapd manager's `status()`:
/// `ap_ssid` from `status()["ssid"]` (the manager's resolved SSID, present
/// regardless of the unit state) and `ap_ip` from `status()["gateway"]`. The front
/// has no in-process manager but reads the same live seams: the resolved SSID off
/// config (the way `ados-net`'s `build_ssid` resolves it) and the
/// `192.168.4.1` gateway while the AP unit is active. `usb_ip` stays the static
/// null leg. `uplink_type` / `uplink_reachable` come from the uplink router's
/// active-uplink sentinel (see [`uplink_view`]). When the AP unit is down, `ap_ip`
/// is null (the manager's status reports the gateway only while up), while
/// `ap_ssid` still resolves off config.
///
/// This is the I/O shell, so it is the frame that goes `async`: the composition
/// stays pure in [`network_view_compose`], which already takes the running flag
/// injected, so there is nothing to hoist out of here and no reason to grow
/// `get_status` a parameter.
pub(super) async fn network_view(state: &AppState) -> Value {
    let cfg = crate::config::load_config_object(&state.pairing_paths.config);
    // The one definition of this liveness probe now lives in
    // `crate::probe::unit_is_active`. It used to be a byte-identical local
    // `hostapd_running()` here AND another in `routes::gs_network`, which is
    // exactly how two surfaces come to give an operator two different answers
    // about the same unit; keep it centralised so it cannot drift again.
    let running = crate::probe::unit_is_active(HOSTAPD_UNIT).await;
    let router_running = crate::probe::unit_is_active(UPLINK_ROUTER_UNIT).await;
    let uplink = uplink_view(&uplink_active_flag_path(&state.run_dir), router_running);
    let identity = crate::config::device_id_file_for(&state.pairing_paths.config);
    network_view_compose(&ap_ssid_from_config(&cfg, &identity), running, uplink)
}

/// `(uplink_type, uplink_reachable)` from the uplink router's sentinel.
///
/// A present sentinel names the selected uplink, mapped to its kind (`eth`,
/// `wifi`, `cellular`, `usb`, else the interface name), with the router's last
/// reachability verdict. An absent sentinel while the router runs is the
/// router's own "no uplink" (`"none"`, unreachable). Anything else (the router
/// not running, an unreadable or malformed file) is unknown: both null, which a
/// reader renders as "—" rather than as offline.
pub(super) fn uplink_view(flag: &Path, router_running: bool) -> (Value, Value) {
    #[derive(Deserialize)]
    struct Flag {
        active_uplink: String,
        internet_reachable: bool,
    }
    match std::fs::read_to_string(flag) {
        Ok(text) => match serde_json::from_str::<Flag>(&text) {
            Ok(f) => {
                let name = f.active_uplink.trim();
                let kind = if name.starts_with("eth") {
                    "eth"
                } else if name.starts_with("wlan") {
                    "wifi"
                } else if name.starts_with("wwan") {
                    "cellular"
                } else if name.starts_with("usb") {
                    "usb"
                } else {
                    name
                };
                (json!(kind), json!(f.internet_reachable))
            }
            Err(_) => (Value::Null, Value::Null),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && router_running => {
            (json!("none"), json!(false))
        }
        Err(_) => (Value::Null, Value::Null),
    }
}

/// Compose the `_network_view` body from the resolved SSID, the live running
/// flag and the uplink verdict. Split out so the shape + the running-vs-not-running
/// gating are unit tested without the `systemctl` IO. `ap_ip` is the gateway while
/// running, else null; `ap_ssid` is the resolved SSID either way.
pub(super) fn network_view_compose(ap_ssid: &str, running: bool, uplink: (Value, Value)) -> Value {
    let (uplink_type, uplink_reachable) = uplink;
    json!({
        "ap_ssid": ap_ssid,
        "ap_ip": if running { Value::String(AP_GATEWAY_IP.to_string()) } else { Value::Null },
        "usb_ip": Value::Null,
        "uplink_type": uplink_type,
        "uplink_reachable": uplink_reachable,
    })
}

/// The resolved AP SSID from config, the way `ados-net`'s `build_ssid`
/// resolves it: honour a configured `network.hotspot.ssid` only when it is
/// non-empty, carries no `{device_id}` placeholder, and already starts with
/// `ADOS-GS-`; otherwise build `ADOS-GS-<first 4 hex of device_id, uppercased,
/// zero-padded>`. The device id resolves against the identity file `identity`.
pub(super) fn ap_ssid_from_config(cfg: &Value, identity: &std::path::Path) -> String {
    let configured = cfg
        .get("network")
        .filter(|v| v.is_object())
        .and_then(|v| v.get("hotspot"))
        .filter(|v| v.is_object())
        .and_then(|h| h.get("ssid"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let configured_id = cfg
        .get("agent")
        .filter(|v| v.is_object())
        .and_then(|v| v.get("device_id"))
        .and_then(Value::as_str);
    resolve_ap_ssid(
        configured,
        &ados_protocol::identity::device_id_at(identity, configured_id),
    )
}

/// Resolve the AP SSID exactly as the live AP owner does (`ados-net`'s
/// configured-SSID gate + `build_ssid`): a configured SSID is
/// honoured only when it is non-empty, has no `{device_id}` placeholder, and
/// already starts with `ADOS-GS-`; otherwise build `ADOS-GS-<short_id>`.
pub(super) fn resolve_ap_ssid(configured: &str, device_id: &str) -> String {
    if !configured.is_empty()
        && !configured.contains("{device_id}")
        && configured.starts_with("ADOS-GS-")
    {
        return configured.to_string();
    }
    format!("ADOS-GS-{}", short_id(device_id))
}

/// The first 4 hex chars of the device id, uppercased, zero-padded to 4 when the
/// id has fewer than 4 hex chars after stripping non-hex characters.
pub(super) fn short_id(device_id: &str) -> String {
    let hex_only: String = device_id.chars().filter(char::is_ascii_hexdigit).collect();
    let padded = if hex_only.len() >= 4 {
        hex_only
    } else {
        format!("{hex_only}0000")
    };
    padded.chars().take(4).collect::<String>().to_uppercase()
}
