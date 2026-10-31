//! The `link` sub-block (live radio link, off wfb-stats.json).

use super::*;

/// The reduced live radio link view the `/status` route carries, sourced from
/// `/run/ados/wfb-stats.json`.
///
/// The ground-station config object has no root `wfb` section (the radio config lives
/// at `video.wfb`), so the Python `getattr(app.config, "wfb", None)` is `None` and the
/// config-channel / tx-power seeds are `None` — this view starts from the same zero
/// base and merges the live snapshot over it. An absent / unparseable / non-object file
/// degrades to the base; a snapshot older than 10 s flips `state` to `"stale"`.
pub(super) fn link_view(run_dir: &Path) -> Value {
    link_view_from(&wfb_stats_path(run_dir))
}

/// The path-injectable core of [`link_view`], reading the live snapshot from an
/// explicit `wfb-stats.json` path. Split out so tests drive it with their own file
/// without mutating the process-global `ADOS_RUN_DIR`.
pub(super) fn link_view_from(path: &Path) -> Value {
    // The base block (what the dashboard renders before the radio writes its first
    // snapshot). config_channel + tx_power are `None` because the GS config carries
    // no root `wfb` section.
    let mut base = Map::new();
    base.insert("rssi_dbm".to_string(), Value::Null);
    base.insert("bitrate_mbps".to_string(), Value::Null);
    base.insert("bitrate_kbps".to_string(), Value::Null);
    base.insert("fec_recovered".to_string(), json!(0));
    base.insert("fec_lost".to_string(), json!(0));
    base.insert("fec_failed".to_string(), json!(0));
    base.insert("channel".to_string(), Value::Null);
    base.insert("snr_db".to_string(), Value::Null);
    base.insert("noise_dbm".to_string(), Value::Null);
    base.insert("packets_received".to_string(), json!(0));
    base.insert("packets_lost".to_string(), json!(0));
    base.insert("loss_percent".to_string(), Value::Null);
    base.insert("tx_power_dbm".to_string(), Value::Null);
    // The radio's declared power path (`host_vbus` / `powered_hub` /
    // `external_5v`), which the panel's brownout warning keys on. Null until the
    // radio writes a snapshot, so no reader guesses a supply topology.
    base.insert("topology".to_string(), Value::Null);
    base.insert("state".to_string(), json!("connecting"));
    // The one-glance link diagnosis (deaf / mis_keyed / jammed / healthy /
    // searching) + the RX counters that separate the failure modes a bare "0
    // received" hides, so the always-on cockpit link bar reads a legible CAUSE.
    base.insert("link_diag".to_string(), Value::Null);
    base.insert("packets_all".to_string(), json!(0));
    base.insert("decrypt_errors".to_string(), json!(0));

    // Live modulation rung + the adaptive ladder's configured cap. `null` until
    // the radio writes its first snapshot — never a confident 0, which would
    // read as MCS 0 (the slowest rung) rather than "not measured yet". The
    // Mission Control Radio settings page renders these beside `snr_db` as the
    // read-only "auto (MCS N at X dB)" row: the ACTIVE rung, not the commanded
    // one, and the cap so a policy-limited rung is not misread as a bad link.
    base.insert("mcs_index".to_string(), Value::Null);
    base.insert("mcs_ladder_cap".to_string(), Value::Null);

    // A future mtime is an unprovable age, not age zero: same rule as
    // [`file_age_s`].
    let Some(age_s) = file_age_s(path, SystemTime::now()) else {
        return Value::Object(base);
    };
    let payload = match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(map)) => map,
            // A well-formed-but-non-object body returns the base, matching the
            // Python `if not isinstance(payload, dict): return base`.
            Ok(_) => return Value::Object(base),
            Err(_) => return Value::Object(base),
        },
        Err(_) => return Value::Object(base),
    };

    // Best-effort schema-drift signal (never reject): warn when the sidecar was
    // written by an agent with a different schema version, then read anyway. The
    // writer const lives in the radio crate, so compare against the shared registry.
    let got = payload.get("version").and_then(Value::as_u64).unwrap_or(0) as u16;
    if let Some(ours) = ados_protocol::contracts::sidecar_version("wfb-stats") {
        ados_protocol::sidecar::check_sidecar_version("wfb-stats", got, ours);
    }

    // Live snapshot wins. `bitrate_mbps` is derived from `bitrate_kbps`;
    // `fec_lost`/`fec_failed` both mirror the payload `fec_failed`; the channel
    // from the file wins over the (null) config.
    let rssi = payload.get("rssi_dbm").filter(|v| v.is_number()).cloned();
    let bitrate_kbps = payload.get("bitrate_kbps").and_then(Value::as_f64);
    let bitrate_mbps = bitrate_kbps.map(|k| round2(k / 1000.0));
    let fec_failed = payload.get("fec_failed").and_then(json_to_i64).unwrap_or(0);

    let mut merged = base;
    merged.insert("rssi_dbm".to_string(), rssi.unwrap_or(Value::Null));
    merged.insert(
        "bitrate_mbps".to_string(),
        bitrate_mbps.map(Value::from).unwrap_or(Value::Null),
    );
    merged.insert(
        "bitrate_kbps".to_string(),
        match bitrate_kbps {
            Some(k) => json!(k as i64),
            None => Value::Null,
        },
    );
    merged.insert(
        "fec_recovered".to_string(),
        json!(payload
            .get("fec_recovered")
            .and_then(json_to_i64)
            .unwrap_or(0)),
    );
    merged.insert("fec_lost".to_string(), json!(fec_failed));
    merged.insert("fec_failed".to_string(), json!(fec_failed));
    // Channel: the tuned channel the payload reports when truthy, else null.
    let payload_channel = payload
        .get("actual_channel")
        .cloned()
        .unwrap_or(Value::Null);
    merged.insert(
        "channel".to_string(),
        if json_truthy(&payload_channel) {
            payload_channel
        } else {
            Value::Null
        },
    );
    merged.insert(
        "snr_db".to_string(),
        payload.get("snr_db").cloned().unwrap_or(Value::Null),
    );
    merged.insert(
        "noise_dbm".to_string(),
        payload.get("noise_dbm").cloned().unwrap_or(Value::Null),
    );
    merged.insert(
        "packets_received".to_string(),
        json!(payload
            .get("packets_received")
            .and_then(json_to_i64)
            .unwrap_or(0)),
    );
    merged.insert(
        "packets_lost".to_string(),
        json!(payload
            .get("packets_lost")
            .and_then(json_to_i64)
            .unwrap_or(0)),
    );
    merged.insert(
        "loss_percent".to_string(),
        payload.get("loss_percent").cloned().unwrap_or(Value::Null),
    );
    // Diagnostic trio from the sidecar: the verdict (a string, null when the
    // writer has not classified yet) + the RX counters (0 when absent).
    merged.insert(
        "link_diag".to_string(),
        payload
            .get("link_diag")
            .filter(|v| v.is_string())
            .cloned()
            .unwrap_or(Value::Null),
    );
    merged.insert(
        "packets_all".to_string(),
        json!(payload
            .get("packets_all")
            .and_then(json_to_i64)
            .unwrap_or(0)),
    );
    merged.insert(
        "decrypt_errors".to_string(),
        json!(payload
            .get("decrypt_errors")
            .and_then(json_to_i64)
            .unwrap_or(0)),
    );
    // Modulation: pass the sidecar's numbers through, `null` when absent.
    merged.insert(
        "mcs_index".to_string(),
        payload
            .get("mcs_index")
            .filter(|v| v.is_number())
            .cloned()
            .unwrap_or(Value::Null),
    );
    merged.insert(
        "mcs_ladder_cap".to_string(),
        payload
            .get("mcs_ladder_cap")
            .filter(|v| v.is_number())
            .cloned()
            .unwrap_or(Value::Null),
    );
    // tx_power_dbm: the payload value when present (not null), else the (null) base.
    let payload_tx = payload.get("tx_power_dbm").cloned();
    merged.insert(
        "tx_power_dbm".to_string(),
        match payload_tx {
            Some(v) if !v.is_null() => v,
            _ => Value::Null,
        },
    );
    merged.insert(
        "topology".to_string(),
        payload
            .get("topology")
            .filter(|v| v.as_str().is_some_and(|s| !s.is_empty()))
            .cloned()
            .unwrap_or(Value::Null),
    );
    // state: the payload value when truthy, else "connecting".
    let payload_state = payload.get("state").cloned().unwrap_or(Value::Null);
    let state = if json_truthy(&payload_state) {
        payload_state
    } else {
        json!("connecting")
    };
    merged.insert("state".to_string(), state);

    // Over the shared freshness ceiling the snapshot is suspect; flip to "stale".
    // This used to be its own `10.0` literal. Two independently-maintained
    // thresholds on sibling surfaces drift, and then two blocks on the same page
    // disagree about whether the same node is stale.
    if age_s > SNAPSHOT_FRESH_S {
        merged.insert("state".to_string(), json!("stale"));
    }
    Value::Object(merged)
}
