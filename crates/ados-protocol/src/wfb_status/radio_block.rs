//! The radio heartbeat block projected from a WFB status view.

use super::*;

/// The RSSI sentinel the link-quality monitor seeds before the first real sample.
/// Treated as "no reading yet" so the radio block reports `null` for it.
pub const RSSI_SENTINEL: f64 = -100.0;

/// The `rf_unverified` verdict forwarded from the radio sidecar, or `null` when it
/// cannot be sourced honestly.
///
/// The radio service owns this boolean (an advancing transmit counter with no
/// confirmed reception inside the proof grace window); every other surface only
/// forwards it, so a consumer never has to re-derive it from a heuristic.
///
/// It reads `null` — never a confident `false` — when the key is absent (a
/// receive-side view, or a sidecar written before the field existed) or when the
/// snapshot went stale, because a reading older than the staleness ceiling cannot
/// say whether the radio is unverified NOW, and a stale `false` is exactly the
/// healthy-looking dead link this field exists to expose.
pub(super) fn rf_unverified_field(status: &Map<String, Value>) -> Value {
    if status.get("state").and_then(Value::as_str) == Some("stale") {
        return Value::Null;
    }
    bool_or_null(status, "rf_unverified")
}

/// Forward a boolean verdict from the status view verbatim, or `null` when the
/// view carries no boolean reading for it.
///
/// The absence of a verdict must never collapse to a confident `false`: the GCS
/// resolves these fields three-state (degraded / ok / unknown), so a fabricated
/// `false` for `adapter_usb_degraded` renders a green "USB link OK" for an adapter
/// nothing ever enumerated, a fabricated `false` for `adapter_injection_ok`
/// renders a red no-injection claim a pre-scan rig never measured, and a
/// fabricated `false` for `phy_muted` asserts a healthy TX PHY on a view (the
/// receive side) that has no TX PHY to read.
pub(super) fn bool_or_null(map: &Map<String, Value>, key: &str) -> Value {
    match map.get(key) {
        Some(Value::Bool(b)) => Value::Bool(*b),
        _ => Value::Null,
    }
}

/// Shape the forward-compatible `radio` heartbeat block from a WFB status view.
///
/// `wfb_status` is the status body (or `None` when no view is available). The GCS
/// keys off the presence of the block, not the values; an absent view yields the
/// [`radio_absent_block`] skeleton.
///
/// The emitted key set is exactly `ados_cloud::heartbeat::RadioBlock`'s 40 fields,
/// order for order, and `ados-cloud`'s `radio_block_from_sidecar` round-trip test
/// pins that: it deserializes this output into the struct and asserts the
/// re-serialization is byte-identical, so a field added here without a matching
/// struct field fails there rather than silently vanishing off the cloud wire.
pub fn build_radio_block(wfb_status: Option<&Map<String, Value>>) -> Value {
    let Some(status) = wfb_status else {
        return radio_absent_block();
    };

    let iface = status.get("interface").and_then(non_empty_string);
    let driver = iface
        .as_deref()
        .and_then(detect_radio_driver_name)
        .map(Value::from)
        .unwrap_or(Value::Null);
    let iface_value = iface.clone().map(Value::from).unwrap_or(Value::Null);

    // The radio block's `channel` is the LIVE interface channel; null when no
    // producer has reported one.
    let channel = status
        .get("actual_channel")
        .filter(|v| !is_falsey(v))
        .cloned()
        .unwrap_or(Value::Null);
    let freq_mhz = channel
        .as_i64()
        .and_then(|c| get_channel(c).map(|ch| ch.frequency_mhz))
        .map(Value::from)
        .unwrap_or(Value::Null);

    let rssi = match status.get("rssi_dbm").and_then(Value::as_f64) {
        Some(v) if v == RSSI_SENTINEL => Value::Null,
        _ => status.get("rssi_dbm").cloned().unwrap_or(Value::Null),
    };
    let bitrate = status
        .get("bitrate_kbps")
        .filter(|v| !is_falsey(v))
        .cloned()
        .unwrap_or(Value::Null);

    json!({
        "state": status.get("state").cloned().unwrap_or(Value::Null),
        "iface": iface_value,
        "driver": driver,
        "channel": channel,
        "freq_mhz": freq_mhz,
        "bandwidth_mhz": 20,
        "tx_power_dbm": get_or_null(status, "tx_power_dbm"),
        "tx_power_max_dbm": get_or_null(status, "tx_power_max_dbm"),
        "topology": get_or_null(status, "topology"),
        "rssi_dbm": rssi,
        "snr_db": get_or_null(status, "snr_db"),
        "noise_dbm": get_or_null(status, "noise_dbm"),
        "bitrate_kbps": bitrate,
        "fec_recovered": get_or_null(status, "fec_recovered"),
        "fec_lost": get_or_null(status, "fec_failed"),
        "packets_lost": get_or_null(status, "packets_lost"),
        "loss_percent": get_or_null(status, "loss_percent"),
        "mcs_index": get_or_null(status, "mcs_index"),
        "rx_silent_seconds": get_or_null(status, "rx_silent_seconds"),
        "paired": json_truthy(status.get("paired").unwrap_or(&Value::Null)),
        "paired_with_device_id": get_or_null(status, "paired_with_device_id"),
        "paired_at": get_or_null(status, "paired_at"),
        "public_key_fingerprint": get_or_null(status, "public_key_fingerprint"),
        "auto_pair_enabled": get_or_null(status, "auto_pair_enabled"),
        "tx_video_stalled": get_or_null(status, "tx_video_stalled"),
        "tx_video_stall_kills": get_or_null(status, "tx_video_stall_kills"),
        "tx_video_recvq_bytes": get_or_null(status, "tx_video_recvq_bytes"),
        "acquire_state": get_or_null(status, "acquire_state"),
        "channel_locked": get_or_null(status, "channel_locked"),
        // The two halves of the received-side proof: `channel_locked` is true once
        // a verified return signal was heard, `rf_unverified` is true when the
        // transmit counter advances while none has been. Forwarded from the
        // radio's own verdict so a consumer reads the authoritative value instead
        // of re-deriving it.
        "rf_unverified": rf_unverified_field(status),
        "reacquire_kills": get_or_null(status, "reacquire_kills"),
        "valid_rx_packets_per_s": get_or_null(status, "valid_rx_packets_per_s"),
        "adapter_chipset": get_or_null(status, "adapter_chipset"),
        // Adapter + PHY verdicts forward verbatim as booleans, or `null` when the
        // view has no reading — never a fabricated false, which the GCS's
        // three-state resolvers would render as a measured green/red claim about
        // hardware this view never examined.
        "adapter_injection_ok": bool_or_null(status, "adapter_injection_ok"),
        "adapter_usb_speed_mbps": get_or_null(status, "adapter_usb_speed_mbps"),
        "adapter_usb_degraded": bool_or_null(status, "adapter_usb_degraded"),
        "phy_muted": bool_or_null(status, "phy_muted"),
        "tx_zombie_kills": get_or_null(status, "tx_zombie_kills"),
        "tx_bytes_per_s": get_or_null(status, "tx_bytes_per_s"),
        "restart_count": get_or_null(status, "restart_count"),
    })
}

/// The "no radio" skeleton every transport carries when there is no WFB status
/// view. Every metric is `null` and `paired` is `false`; the adapter / PHY
/// verdicts are `null` too — with no radio view there is nothing to have measured
/// them, and a `false` would claim a healthy USB link / unmuted PHY that was never
/// examined.
///
/// Three keys of the populated block are deliberately ABSENT rather than null
/// (`tx_zombie_kills`, `tx_bytes_per_s`, `restart_count`): they are the
/// `skip_serializing_if` fields on the cloud `RadioBlock`, and the frozen
/// heartbeat fixture pins the 37-key absent form exactly.
pub fn radio_absent_block() -> Value {
    json!({
        "state": "absent",
        "iface": null,
        "driver": null,
        "channel": null,
        "freq_mhz": null,
        "bandwidth_mhz": null,
        "tx_power_dbm": null,
        "tx_power_max_dbm": null,
        "topology": null,
        "rssi_dbm": null,
        "snr_db": null,
        "noise_dbm": null,
        "bitrate_kbps": null,
        "fec_recovered": null,
        "fec_lost": null,
        "packets_lost": null,
        "loss_percent": null,
        "mcs_index": null,
        "rx_silent_seconds": null,
        "paired": false,
        "paired_with_device_id": null,
        "paired_at": null,
        "public_key_fingerprint": null,
        "auto_pair_enabled": null,
        "tx_video_stalled": null,
        "tx_video_stall_kills": null,
        "tx_video_recvq_bytes": null,
        "acquire_state": null,
        "channel_locked": null,
        // Null, not false: with no radio view there is no verdict to report, and a
        // false here would claim a transmit path was proven.
        "rf_unverified": null,
        "reacquire_kills": null,
        "valid_rx_packets_per_s": null,
        "adapter_chipset": null,
        "adapter_injection_ok": null,
        "adapter_usb_speed_mbps": null,
        "adapter_usb_degraded": null,
        "phy_muted": null,
    })
}

/// Best-effort kernel driver name for the WFB monitor interface, read from
/// `/sys/class/net/<iface>/device/uevent`'s `DRIVER=` line. `None` for an empty
/// iface or an unreadable file.
pub(super) fn detect_radio_driver_name(interface: &str) -> Option<String> {
    if interface.is_empty() {
        return None;
    }
    let path = Path::new("/sys/class/net")
        .join(interface)
        .join("device")
        .join("uevent");
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("DRIVER=") {
            let name = rest.trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}
