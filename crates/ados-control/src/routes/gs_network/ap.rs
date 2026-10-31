//! The AP leg of the aggregate network view: hostapd state, AP guard, station list.

use super::*;

/// The AP leg, reproducing the Python `_ap_view` over the live hostapd state.
///
/// The Python `_ap_view` reaches its live `try` branch on any ground station (the
/// hostapd `status()` never raises) and returns that manager's live status:
/// `running` from `systemctl is-active ados-hostapd`, the resolved SSID, the
/// channel, the AP interface, the `192.168.4.1` gateway, and the associated
/// station MACs. The front has no in-process hostapd manager, but every one of
/// those legs reads off the same live seams the manager reads, so the front
/// probes them directly instead of reporting a static not-running shape.
///
/// - `running` / `enabled` — `systemctl is-active ados-hostapd.service == active`.
/// - `ssid` — resolved the way `ados-net`'s `build_ssid` resolves it: a
///   configured SSID is honoured only when it is non-empty, carries no
///   `{device_id}` placeholder, and is already an `ADOS-GS-` name; otherwise it
///   is built as `ADOS-GS-<first 4 hex of device_id, uppercased, zero-padded>`.
/// - `channel` — the configured hotspot channel, defaulting to `6` (the manager's
///   default), so the channel is always an integer.
/// - `interface` — the configured AP interface, defaulting to `wlan0`.
/// - `gateway` — `192.168.4.1` while the AP is running, else null (the manager's
///   `status()` reports the gateway only when the unit is up).
/// - `connected_clients` — the associated station MACs from
///   `iw dev <iface> station dump` while running, else the empty list.
///
/// `async` rather than taking the two live legs as injected parameters: the pure
/// composition is already split out into `ap_view_compose`, so what is left here
/// IS the I/O shell, and hoisting the probes into the handler would have to hoist
/// `ap_interface(cfg)` with them (the `iw` probe needs the resolved interface) —
/// moving config derivation out of the view and buying no testability, since the
/// shape is already tested through `ap_view_compose`.
pub(super) async fn ap_view(cfg: &Value) -> Value {
    let hotspot = cfg
        .get("network")
        .filter(|v| v.is_object())
        .and_then(|v| v.get("hotspot"))
        .filter(|v| v.is_object());
    let configured_ssid = hotspot
        .and_then(|h| h.get("ssid"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let configured_id = cfg
        .get("agent")
        .filter(|v| v.is_object())
        .and_then(|v| v.get("device_id"))
        .and_then(Value::as_str);
    let device_id = ados_protocol::identity::device_id(configured_id);
    let ssid = resolve_ap_ssid(configured_ssid, &device_id);
    let channel = hotspot
        .and_then(|h| h.get("channel"))
        .and_then(json_to_i64)
        .unwrap_or(AP_DEFAULT_CHANNEL);
    let interface = ap_interface(cfg);

    // `systemctl is-active ados-hostapd.service`. A probe that got no answer
    // (no `systemctl`, a spawn error, a timeout) is unknown, not down.
    let running = crate::probe::unit_state(HOSTAPD_UNIT).await;
    let clients = if running == Some(true) {
        station_dump_macs(&interface).await
    } else {
        Vec::new()
    };
    let mut view = ap_view_compose(&ssid, channel, &interface, running, clients);
    merge_ap_guard_diagnostics(&mut view);
    view
}

/// Merge the setup-AP guard's live decision (`standing_down` + reason + phy count +
/// client-uplink flag) into the AP view so an operator can see WHY the AP is up or down.
/// The count + client flag pass through verbatim (a JSON number / bool, or null
/// when absent); an absent or unreadable sidecar reads as not standing down,
/// reason `unknown`.
pub(super) fn merge_ap_guard_diagnostics(view: &mut Value) {
    let Some(obj) = view.as_object_mut() else {
        return;
    };
    let guard = crate::sidecar::read_object(Path::new(AP_GUARD_SIDECAR)).unwrap_or_default();
    let standing_down = guard
        .get("standing_down")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let reason = guard
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let phy_count = guard.get("wifi_phy_count").cloned().unwrap_or(Value::Null);
    let client_up = guard
        .get("client_uplink_active")
        .cloned()
        .unwrap_or(Value::Null);
    obj.insert("standing_down".to_string(), Value::Bool(standing_down));
    obj.insert("standdown_reason".to_string(), Value::String(reason));
    obj.insert("wifi_phy_count".to_string(), phy_count);
    obj.insert("client_uplink_active".to_string(), client_up);
}

/// Compose the `_ap_view` body from already-probed pieces: the resolved SSID, the
/// channel, the AP interface, the live running flag, and the associated station MACs.
/// Split out so the shape + the running-vs-not-running gating are unit tested without
/// the `systemctl` / `iw` IO.
pub(super) fn ap_view_compose(
    ssid: &str,
    channel: i64,
    interface: &str,
    running: Option<bool>,
    clients: Vec<String>,
) -> Value {
    let up = running == Some(true);
    json!({
        "enabled": running,
        "running": running,
        "ssid": ssid,
        "channel": channel,
        "interface": interface,
        "gateway": if up { Value::String(AP_GATEWAY_IP.to_string()) } else { Value::Null },
        "connected_clients": clients,
    })
}

/// The AP interface, resolved the way the hostapd manager resolves it.
///
/// Reporting the configured value, or a literal when none is set, made this a
/// surface asserting something nothing had checked: the manager binds
/// `resolve_ap_interface(hotspot.interface)`, which classifies by driver and
/// refuses the flight radio, so on a default-config box the two disagreed
/// exactly when it mattered. The configured value is still the input, and is
/// still honoured when it names a real non-radio interface.
pub(super) fn ap_interface(cfg: &Value) -> String {
    let configured = cfg
        .get("network")
        .filter(|v| v.is_object())
        .and_then(|v| v.get("hotspot"))
        .filter(|v| v.is_object())
        .and_then(|h| h.get("interface"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or_default();

    ados_protocol::netif::resolve_ap_interface(
        configured,
        ados_protocol::netif::radio_interface().as_deref(),
    )
    .unwrap_or_else(|_| {
        // Resolution refused (the configured name is the radio, or the box has
        // no onboard WiFi). The manager falls back the same way rather than
        // failing the whole status read.
        if configured.is_empty() {
            AP_IFACE.to_string()
        } else {
            configured.to_string()
        }
    })
}

/// The associated station MACs from `iw dev <iface> station dump`, parsing the
/// `Station <mac> (on <iface>)` header lines and lowercasing each MAC, reproducing
/// the manager's `_connected_clients`. A missing `iw` / a non-zero exit / a spawn
/// error / a timeout all yield the empty list, matching the manager's `except` /
/// `not ok` returns.
pub(super) async fn station_dump_macs(interface: &str) -> Vec<String> {
    parse_station_dump(
        crate::probe::capture(
            "iw",
            &["dev", interface, "station", "dump"],
            crate::probe::PROBE_TIMEOUT,
        )
        .await
        .text(),
    )
}

/// Parse the MAC addresses out of `iw … station dump` output: each associated
/// station begins with a `Station <mac> …` line whose second whitespace token is
/// the MAC, lowercased.
pub(super) fn parse_station_dump(text: &str) -> Vec<String> {
    let mut macs = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Station ") {
            if let Some(mac) = rest.split_whitespace().next() {
                macs.push(mac.to_lowercase());
            }
        }
    }
    macs
}

/// Resolve the AP SSID the way the live hostapd manager does: honor a configured SSID
/// only when it is non-empty, has no `{device_id}` placeholder, and already starts with
/// `ADOS-GS-`; otherwise build `ADOS-GS-<short_id>` from the device id.
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
