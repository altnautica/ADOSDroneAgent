//! Pure mappers from agent JSON documents to page context structs.

use super::*;

/// Borrow a child object/value by key, when present.
pub(super) fn get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.get(key)
}

/// A string field on `v`, when it is present and a JSON string.
pub(super) fn string_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

/// A float field on `v`, accepting integer or float JSON numbers.
pub(super) fn f64_field(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64)
}

/// An integer field on `v`. Truncates a float number toward zero.
pub(super) fn i64_field(v: &Value, key: &str) -> Option<i64> {
    let n = v.get(key)?;
    n.as_i64().or_else(|| n.as_f64().map(|f| f as i64))
}

/// A boolean field on `v`, defaulting to `false` when absent or non-bool.
pub(super) fn bool_field(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool).unwrap_or(false)
}

// ── per-context mappers ─────────────────────────────────────────────

/// The `link` block as the pages read it.
///
/// A `stale` block keeps only its state. The agent flips a snapshot older than
/// its freshness window to `stale` but still carries the dead producer's last
/// RSSI, bitrate, FEC, channel and TX power; painting those would show a dead
/// radio as a live one, so every measured field is dropped here, once, for
/// every page.
pub(super) fn link_ctx(v: Option<&Value>) -> LinkCtx {
    let Some(v) = v else {
        return LinkCtx::default();
    };
    let state = string_field(v, "state");
    // The declared power path is configuration, not a reading, so it survives a
    // stale snapshot.
    let topology = string_field(v, "topology");
    if state.as_deref() == Some(LINK_STATE_STALE) {
        return LinkCtx {
            state,
            topology,
            ..LinkCtx::default()
        };
    }
    LinkCtx {
        state,
        rssi_dbm: f64_field(v, "rssi_dbm"),
        snr_db: f64_field(v, "snr_db"),
        noise_dbm: f64_field(v, "noise_dbm"),
        loss_percent: f64_field(v, "loss_percent"),
        // `bitrate_kbps` is canonical; a producer that only carries the
        // megabit shim still resolves.
        bitrate_kbps: f64_field(v, "bitrate_kbps")
            .or_else(|| f64_field(v, "bitrate_mbps").map(|mbps| mbps * 1000.0)),
        // The producer key is `fec_failed`; the view also mirrors it as
        // `fec_lost`. Read either so both producer + view shapes resolve.
        fec_recovered: i64_field(v, "fec_recovered"),
        fec_lost: i64_field(v, "fec_lost").or_else(|| i64_field(v, "fec_failed")),
        channel: i64_field(v, "channel"),
        frequency_mhz: i64_field(v, "frequency_mhz"),
        bandwidth_mhz: i64_field(v, "bandwidth_mhz"),
        tx_power_dbm: i64_field(v, "tx_power_dbm"),
        topology,
        mcs_index: i64_field(v, "mcs_index"),
        fec_k: i64_field(v, "fec_k"),
        fec_n: i64_field(v, "fec_n"),
        // Option<bool> (not the defaulted bool_field) so the page can tell a
        // reported `false` from an older agent that omits the field.
        adaptive_bitrate_enabled: v.get("adaptive_bitrate_enabled").and_then(Value::as_bool),
        recommended_tier_name: string_field(v, "recommended_tier_name"),
        packets_received: i64_field(v, "packets_received"),
        packets_lost: i64_field(v, "packets_lost"),
        rssi_history: Vec::new(),
    }
}

pub(super) fn drone_ctx(v: Option<&Value>) -> DroneCtx {
    let Some(v) = v else {
        return DroneCtx::default();
    };
    DroneCtx {
        device_id: string_field(v, "device_id"),
        fc_mode: string_field(v, "fc_mode"),
        battery_pct: f64_field(v, "battery_pct"),
        gps_sats: i64_field(v, "gps_sats"),
        armed: v.get("armed").and_then(Value::as_bool),
        key_fingerprint: string_field(v, "key_fingerprint"),
    }
}

pub(super) fn paired_drone_ctx(v: Option<&Value>) -> PairedDroneCtx {
    let Some(v) = v else {
        return PairedDroneCtx::default();
    };
    PairedDroneCtx {
        device_id: string_field(v, "device_id"),
        key_fingerprint: string_field(v, "key_fingerprint"),
        paired_at_seconds: f64_field(v, "paired_at_seconds"),
        paired_at: f64_field(v, "paired_at"),
        auto_pair_enabled: None,
    }
}

/// The WFB auto-pair arm flag off a `GET /api/wfb/pair` body. `None` when the
/// read failed or the body carries no boolean flag.
pub(super) fn auto_pair_enabled(wfb_pair: Option<&Value>) -> Option<bool> {
    wfb_pair?.get("auto_pair_enabled").and_then(Value::as_bool)
}

pub(super) fn fc_ctx(v: Option<&Value>) -> FcCtx {
    let Some(v) = v else {
        return FcCtx::default();
    };
    FcCtx {
        vehicle: string_field(v, "vehicle"),
        mode: string_field(v, "mode"),
        armed: v.get("armed").and_then(Value::as_bool),
        battery_voltage: f64_field(v, "battery_voltage"),
        battery_remaining: f64_field(v, "battery_remaining"),
        gps_fix_type: i64_field(v, "gps_fix_type"),
        gps_satellites_visible: i64_field(v, "gps_satellites_visible"),
        battery_history: Vec::new(),
    }
}

pub(super) fn cloud_ctx(v: Option<&Value>) -> CloudCtx {
    let Some(v) = v else {
        return CloudCtx::default();
    };
    CloudCtx {
        paired: bool_field(v, "paired"),
        // One pair-code field whichever spelling the producer used.
        pair_code: string_field(v, "pair_code").or_else(|| string_field(v, "pairing_code")),
        latency_ms: f64_field(v, "latency_ms"),
        rtt_ms: f64_field(v, "rtt_ms"),
        broadcasting: bool_field(v, "broadcasting"),
        mqtt_state: string_field(v, "mqtt_state"),
        http_state: string_field(v, "http_state"),
        drone_id: string_field(v, "drone_id"),
    }
}

pub(super) fn pairing_ctx(v: Option<&Value>) -> PairingCtx {
    let Some(v) = v else {
        return PairingCtx::default();
    };
    PairingCtx {
        code: string_field(v, "code"),
        window_active: bool_field(v, "window_active"),
        window_remaining_seconds: f64_field(v, "window_remaining_seconds"),
    }
}

pub(super) fn role_ctx(v: Option<&Value>) -> RoleCtx {
    let Some(v) = v else {
        return RoleCtx::default();
    };
    RoleCtx {
        current: string_field(v, "current"),
        configured: string_field(v, "configured"),
        mesh_capable: bool_field(v, "mesh_capable"),
    }
}

/// The mesh block. The agent reports `up` / `peer_count` as null (and
/// `stale: true`) when it has no current snapshot; those stay unknown here
/// rather than becoming "down" and "0 peers". The roster is `None` when the
/// block carries no `peers` array, which is different from an empty one.
pub(super) fn mesh_ctx(v: Option<&Value>) -> MeshCtx {
    let Some(v) = v else {
        return MeshCtx::default();
    };
    let peers = v
        .get("peers")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().map(mesh_peer).collect());
    MeshCtx {
        up: v.get("up").and_then(Value::as_bool),
        stale: bool_field(v, "stale"),
        partition: bool_field(v, "partition"),
        peer_count: i64_field(v, "peer_count"),
        selected_gateway: string_field(v, "selected_gateway"),
        mesh_id: string_field(v, "mesh_id"),
        peers,
    }
}

pub(super) fn mesh_peer(v: &Value) -> MeshPeer {
    MeshPeer {
        device_id: string_field(v, "device_id"),
        role: string_field(v, "role"),
        last_seen_seconds_ago: f64_field(v, "last_seen_seconds_ago"),
    }
}

pub(super) fn network_ctx(v: Option<&Value>) -> NetworkCtx {
    let Some(v) = v else {
        return NetworkCtx::default();
    };
    let wifi_client = v
        .get("wifi_client")
        .map(|w| WifiClientCtx {
            connected: bool_field(w, "connected"),
            ssid: string_field(w, "ssid"),
            signal_dbm: f64_field(w, "signal_dbm").or_else(|| f64_field(w, "signal")),
        })
        .unwrap_or_default();
    NetworkCtx {
        ap_ssid: string_field(v, "ap_ssid"),
        ap_ip: string_field(v, "ap_ip"),
        usb_ip: string_field(v, "usb_ip"),
        uplink_type: string_field(v, "uplink_type"),
        uplink_reachable: v.get("uplink_reachable").and_then(Value::as_bool),
        hotspot_ssid: string_field(v, "hotspot_ssid"),
        // Read from disk, never from the polled response: those routes answer
        // the LAN as well as loopback, so a passphrase in a payload is a
        // passphrase published to anyone who can reach the box.
        ap_passphrase: read_ap_passphrase(),
        wifi_client,
    }
}

/// The access point's passphrase, from its 0600 file on this box.
///
/// `None` for absent or unreadable — the page says so rather than rendering a
/// blank, because a blank where a passphrase belongs reads as "there isn't one"
/// and sends the operator looking for a network that needs no password.
pub(super) fn read_ap_passphrase() -> Option<String> {
    let path =
        std::env::var("ADOS_AP_PASSPHRASE_PATH").unwrap_or_else(|_| AP_PASSPHRASE_PATH.to_string());
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

pub(super) fn uplink_ctx(v: Option<&Value>) -> UplinkCtx {
    let Some(v) = v else {
        return UplinkCtx::default();
    };
    // The status modem block reports presence via `connected` / `enabled`;
    // accept an explicit `modem_present` first, then fall back to those.
    let modem_present = v
        .get("modem_present")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| bool_field(v, "connected") || bool_field(v, "enabled"));
    UplinkCtx {
        modem_present,
        rsrp_dbm: f64_field(v, "rsrp_dbm"),
        rsrq_db: f64_field(v, "rsrq_db"),
        sinr_db: f64_field(v, "sinr_db"),
        band: string_field(v, "band"),
        ip: string_field(v, "ip"),
        tech: string_field(v, "tech").or_else(|| string_field(v, "technology")),
        reason: string_field(v, "reason"),
    }
}

pub(super) fn system_ctx(v: Option<&Value>) -> SystemCtx {
    let Some(v) = v else {
        return SystemCtx::default();
    };
    SystemCtx {
        cpu_pct: f64_field(v, "cpu_pct"),
        ram_used_mb: f64_field(v, "ram_used_mb"),
        ram_total_mb: f64_field(v, "ram_total_mb"),
        temp_c: f64_field(v, "temp_c"),
        disk_pct: f64_field(v, "disk_pct"),
        uptime_seconds: f64_field(v, "uptime_seconds"),
        agent_version: string_field(v, "agent_version"),
        cpu_history: Vec::new(),
        temp_history: Vec::new(),
    }
}

pub(super) fn video_ctx(v: Option<&Value>) -> VideoCtx {
    let Some(v) = v else {
        return VideoCtx::default();
    };
    VideoCtx {
        decoder: string_field(v, "decoder"),
        active: v.get("active").and_then(Value::as_bool),
        recording: bool_field(v, "recording"),
        fps: f64_field(v, "fps"),
        latency_ms: f64_field(v, "latency_ms"),
        bitrate_kbps: f64_field(v, "bitrate_kbps"),
        mediamtx_ready: v.get("mediamtx_ready").and_then(Value::as_bool),
        mediamtx_inbound_kbps: f64_field(v, "mediamtx_inbound_kbps"),
        camera_label: string_field(v, "camera_label"),
        camera_count: i64_field(v, "camera_count").unwrap_or(0),
    }
}

/// The setup status `hardware_check` block (`{profile, items: [...]}`), each
/// item carrying the producer's `id`, `label`, `required`, `state` and
/// `fix_hint`.
pub(super) fn hardware_items(v: Option<&Value>) -> Vec<HardwareItem> {
    let Some(arr) = v.and_then(|v| v.get("items")).and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .map(|item| HardwareItem {
            id: string_field(item, "id"),
            label: string_field(item, "label"),
            required: bool_field(item, "required"),
            state: string_field(item, "state"),
            fix_hint: string_field(item, "fix_hint"),
        })
        .collect()
}

/// The Mission Control URL the setup service advertises: the `access_urls`
/// entry of kind `mission_control`.
pub(super) fn mission_control_url(su: &Value) -> Option<String> {
    su.get("access_urls")?
        .as_array()?
        .iter()
        .find(|u| u.get("kind").and_then(Value::as_str) == Some("mission_control"))
        .and_then(|u| string_field(u, "url"))
}

/// Apply the setup-status snapshot onto the context: completion percent, the
/// next-step copy, the wizard-finalized flag, the LAN host and Mission Control
/// URL the setup and pairing links use, the hardware checklist, and the local
/// pair code the dashboard shows before a cloud relay binds one.
pub(super) fn apply_setup(ctx: &mut PageContext, su: &Value) {
    ctx.setup_finalized = bool_field(su, "finalized")
        || bool_field(su, "setup_complete")
        || bool_field(su, "complete");
    ctx.completion_percent =
        f64_field(su, "completion_percent").or_else(|| f64_field(su, "percent"));
    ctx.next_action = string_field(su, "next_action").or_else(|| string_field(su, "next_step"));
    ctx.lan_host = string_field(su, "lan_host");
    ctx.mission_control_url = mission_control_url(su);
    ctx.hardware_check = hardware_items(get(su, "hardware_check"));

    // A local pair code carried on the setup snapshot seeds the pairing +
    // cloud code surfaces when the status block didn't already populate them.
    if let Some(code) = string_field(su, "pairing_code").or_else(|| string_field(su, "pair_code")) {
        if ctx.pairing.code.is_none() {
            ctx.pairing.code = Some(code.clone());
        }
        if ctx.cloud.pair_code.is_none() {
            ctx.cloud.pair_code = Some(code);
        }
    }
    if !ctx.cloud.paired {
        ctx.cloud.paired = bool_field(su, "paired");
    }
}

/// Build the channel-hopping context from a fresh `hop-supervisor.json` (the
/// caller drops a stale one). The reference channel is the live radio channel
/// taken from the link block.
pub(super) fn hopping_ctx(hop: Option<&Value>, link_channel: Option<i64>) -> HoppingCtx {
    let Some(v) = hop else {
        return HoppingCtx {
            radio_channel: link_channel,
            ..HoppingCtx::default()
        };
    };
    let history = v
        .get("history")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(hop_entry).collect())
        .unwrap_or_default();
    HoppingCtx {
        present: true,
        band: string_field(v, "band"),
        history,
        radio_channel: link_channel,
    }
}

/// One hop-history row. The producer keys are `at` / `from` / `to` / `ok` /
/// `trigger`; a row missing any of the first four is dropped, matching the
/// Python page's `_history` validity filter.
pub(super) fn hop_entry(v: &Value) -> Option<HopEntry> {
    let at = f64_field(v, "at")?;
    let from_channel = i64_field(v, "from")?;
    let to_channel = i64_field(v, "to")?;
    let ok = v.get("ok").and_then(Value::as_bool)?;
    Some(HopEntry {
        at,
        from_channel,
        to_channel,
        ok,
        trigger: string_field(v, "trigger"),
    })
}

// ── disk + OS helpers ───────────────────────────────────────────────
