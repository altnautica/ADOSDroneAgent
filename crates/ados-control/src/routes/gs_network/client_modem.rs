//! The Wi-Fi client and cellular modem legs of the aggregate network view.

use super::*;

/// The Wi-Fi client leg. Reads the live station status from the `ados-net`
/// command socket's `wifi_status` op and the `enabled_on_boot` flag from the
/// client config file, reshaping to the `_wifi_client_view` shape
/// `{enabled_on_boot, connected, ssid, signal, ip}`. An unreachable socket
/// degrades to the full default the Python `except` returns.
pub(super) async fn wifi_client_view() -> Value {
    let status = wifi_status(&wifi_cmd_sock()).await;
    let enabled_on_boot = crate::sidecar::read_object(&gs_wifi_client_json())
        .and_then(|m| m.get("enabled_on_boot").map(json_truthy))
        .unwrap_or(false);
    wifi_client_view_compose(status.as_ref(), enabled_on_boot)
}

/// Compose the Wi-Fi client leg. `enabled_on_boot` is a config fact and is
/// always reported. The live legs come from the net daemon's status reply; when
/// the daemon did not answer, or its reply omits `connected`, the station state
/// is unknown and reads `null`, never a fabricated "disconnected".
pub(super) fn wifi_client_view_compose(
    status: Option<&Map<String, Value>>,
    enabled_on_boot: bool,
) -> Value {
    let field = |k: &str| {
        status
            .and_then(|s| s.get(k))
            .cloned()
            .unwrap_or(Value::Null)
    };
    json!({
        "enabled_on_boot": enabled_on_boot,
        "connected": status
            .and_then(|s| s.get("connected"))
            .map(json_truthy)
            .map(Value::Bool)
            .unwrap_or(Value::Null),
        "ssid": field("ssid"),
        "signal": field("signal"),
        "ip": field("ip"),
    })
}

/// The modem leg of the aggregate view (also the `GET .../network/modem` body).
///
/// Two kinds of field, kept distinct:
///
/// - **Configured** (`enabled`, `apn`, `cap_mb`) — read from the modem config
///   file. A real answer even when no modem is attached, because it describes
///   what the operator asked for, not what the hardware is doing.
/// - **Measured** (`connected`, `iface`, `ip`, `signal_quality`, `technology`,
///   `operator`, `state`, `data_used_mb`, `percent`) — `null` unless something
///   reported them. The front has no `mmcli` polling seam, so the connectivity
///   legs are always `null` here; the usage legs come from the store's
///   most-recent `net.modem_usage` event when it has one.
///
/// They used to be run together: `connected:false`, `iface:"wwan0"`,
/// `signal_quality:-1`, `technology:"unknown"`, `operator:""`,
/// `state:"disconnected"` and `data_used_mb:0` were emitted unconditionally, as
/// the values a live `ModemManager.status()` returns when it has looked and found
/// no modem. This front never looked. So a working modem read as disconnected
/// with a named interface and a zero data allowance consumed — and
/// `signal_quality: -1` is the textbook absent-data-as-plausible-measurement
/// sentinel the GCS then renders on a bar meter.
///
/// Shared with the `PUT .../network/modem` write route, which returns the same
/// `_modem_view()` body after persisting the config sidecar, so the PUT response
/// is the GET response over the freshly-persisted config.
pub(crate) async fn modem_view(state: &AppState) -> Value {
    let cfg = crate::sidecar::read_object(&gs_modem_json()).unwrap_or_default();
    modem_body(&cfg, latest_modem_usage(state).await.as_ref())
}

/// Compose the modem body from the modem config object and the store's usage
/// block. Pure, so the configured-vs-measured split is asserted against this
/// function rather than against a copy of it written inside a test.
pub(super) fn modem_body(cfg: &Map<String, Value>, usage: Option<&Map<String, Value>>) -> Value {
    let enabled = cfg.get("enabled").map(json_truthy).unwrap_or(false);
    let apn = cfg
        .get("apn")
        .filter(|v| v.is_string())
        .cloned()
        .unwrap_or(Value::Null);

    // cap_mb from the config cap_gb, mirroring the Python int(float(cap_gb)*1024).
    // A configured cap is a config value, so an absent one is `null` — no cap
    // configured is not a cap of 0 MB, which reads as "no data allowed".
    let mut cap_mb: Value = cfg
        .get("cap_gb")
        .and_then(json_to_f64)
        .map(|gb| Value::from((gb * 1024.0) as i64))
        .unwrap_or(Value::Null);
    let mut data_used_mb = Value::Null;
    let mut percent = Value::Null;

    // Store overlay: the daemon's data-cap tracker ships the cumulative usage as
    // a net.modem_usage event; a hit is the only source of a real usage figure.
    if let Some(usage) = usage {
        if let Some(v) = usage.get("data_used_mb").and_then(json_to_f64) {
            data_used_mb = Value::from(v as i64);
        }
        if let Some(v) = usage.get("cap_mb").and_then(json_to_f64) {
            cap_mb = Value::from(v as i64);
        }
        if let Some(v) = usage.get("percent").and_then(json_to_f64) {
            percent = Value::from(round2(v));
        }
    }

    json!({
        "enabled": enabled,
        "apn": apn,
        "cap_mb": cap_mb,
        // Unprobed on this front: no modem-status seam exists here.
        "connected": Value::Null,
        "iface": Value::Null,
        "ip": Value::Null,
        "signal_quality": Value::Null,
        "technology": Value::Null,
        "operator": Value::Null,
        "state": Value::Null,
        "data_used_mb": data_used_mb,
        "percent": percent,
    })
}
