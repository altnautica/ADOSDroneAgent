//! The flight-controller and service-fleet enrichment folds.

use super::*;

/// Fold the FC-link fields in from one bounded read of the state IPC socket. The
/// router publishes the vehicle snapshot with `fc_connected` / `fc_port` /
/// `fc_baud` extras; this connects, reads one snapshot frame, and lifts those
/// fields. On a timeout / absent socket / parse error the fields are omitted.
pub(super) fn fold_fc(obj: &mut Map<String, Value>) {
    let snap = match read_state_snapshot() {
        Some(v) => v,
        None => return,
    };
    let map = match snap.as_object() {
        Some(m) => m,
        None => return,
    };

    if let Some(connected) = map.get("fc_connected").and_then(Value::as_bool) {
        obj.insert("fcConnected".to_string(), json!(connected));
    }
    // The honest connected-or-reachable verdict: true for a healthy MSP flight
    // controller (Betaflight/iNav), which never emits the MAVLink heartbeat
    // fc_connected gates on. The router computes it once from the same inputs
    // as fc_connected, so every consumer — the direct-connect status route and
    // this cloud/relay path alike — reads the identical answer.
    if let Some(reachable) = map.get("fc_reachable").and_then(Value::as_bool) {
        obj.insert("fcReachable".to_string(), json!(reachable));
    }
    if let Some(port) = map.get("fc_port").and_then(Value::as_str) {
        if !port.is_empty() {
            obj.insert("fcPort".to_string(), json!(port));
        }
    }
    if let Some(baud) = map.get("fc_baud").and_then(Value::as_i64) {
        if baud > 0 {
            obj.insert("fcBaud".to_string(), json!(baud));
        }
    }
    // The gated-truth detail behind fcConnected: transport-open vs alive, the
    // heartbeat age, the configured transport class, and the not-alive hint. The
    // GCS already renders these on the LAN path; carry them over cloud relay too
    // so a cloud-paired drone can show "port open · no MAVLink" + the hint rather
    // than only a connected boolean.
    if let Some(open) = map.get("transport_open").and_then(Value::as_bool) {
        obj.insert("transportOpen".to_string(), json!(open));
    }
    if let Some(alive) = map.get("mavlink_alive").and_then(Value::as_bool) {
        obj.insert("mavlinkAlive".to_string(), json!(alive));
    }
    if let Some(age) = map.get("heartbeat_age_s").and_then(Value::as_f64) {
        obj.insert("heartbeatAgeS".to_string(), json!(age));
    }
    if let Some(source) = map.get("fc_source").and_then(Value::as_str) {
        if !source.is_empty() {
            obj.insert("fcSource".to_string(), json!(source));
        }
    }
    if let Some(hint) = map.get("fc_link_hint").and_then(Value::as_str) {
        if !hint.is_empty() {
            obj.insert("fcLinkHint".to_string(), json!(hint));
        }
    }
    // Whether the open FC source runs telemetry-only with its host->FC
    // command-down direction gated closed (a MAVLink-over-ELRS ingest). Carry it
    // over cloud relay so a cloud-paired GCS renders "commands gated" rather than
    // a bare connected boolean and never assumes a command reached the FC.
    if let Some(gated) = map.get("fc_command_down_gated").and_then(Value::as_bool) {
        obj.insert("fcCommandDownGated".to_string(), json!(gated));
    }
    // The FC firmware family (betaflight/inav) from the USB descriptor, so a
    // cloud-relay GCS can badge an MSP FC instead of showing "not connected".
    if let Some(variant) = map.get("fc_variant").and_then(Value::as_str) {
        if !variant.is_empty() {
            obj.insert("fcVariant".to_string(), json!(variant));
        }
    }
    // The canonical firmware family (ardupilot/px4/betaflight/inav), so a
    // cloud-relay GCS can badge the family — including ArduPilot vs PX4, which
    // fcVariant cannot distinguish. Omit the "unknown"/empty non-badge states.
    if let Some(firmware) = map.get("fc_firmware").and_then(Value::as_str) {
        if !firmware.is_empty() && firmware != "unknown" {
            obj.insert("fcFirmware".to_string(), json!(firmware));
        }
    }
}

/// Read one state snapshot from the state IPC socket. The state socket replays
/// its last buffer on connect, so a single frame arrives immediately; the read
/// is wall-bounded by [`STATE_READ_TIMEOUT`] so a stalled or absent router never
/// holds the heartbeat tick. The shared reader auto-detects the state wire form
/// per frame (v1 newline-JSON or v2 length-prefixed msgpack), so this reads a v2
/// snapshot as well — the installer ships v2, and a v1-only read silently yields
/// nothing against it.
pub(super) fn read_state_snapshot() -> Option<Value> {
    let path = std::env::var("ADOS_STATE_SOCK").unwrap_or_else(|_| STATE_SOCK.to_string());
    let mut stream = UnixStream::connect(Path::new(&path)).ok()?;
    stream.set_read_timeout(Some(STATE_READ_TIMEOUT)).ok()?;

    // One frame off the replayed buffer via the shared blocking reader. Any
    // timeout / clean EOF / framing error collapses to `None` so a stalled or
    // absent router never holds the tick (the same tolerance the byte-loop had).
    read_state_value_blocking(&mut stream).ok().flatten()
}

/// Fold the `ados-*` service fleet in from one `systemctl list-units`. Omits the
/// `services` key when systemctl is absent / errors. Each entry is the Convex
/// service-object shape reduced to what `list-units` actually reports
/// (`name`/`status`).
pub(super) fn fold_services(obj: &mut Map<String, Value>) {
    let out = match run_with_timeout(
        "systemctl",
        &[
            "list-units",
            "--type=service",
            "--all",
            "--no-pager",
            "--no-legend",
            "ados-*.service",
        ],
        SYSTEMCTL_TIMEOUT,
    ) {
        Some(o) => o,
        None => return,
    };
    let services = parse_systemctl_units(&out);
    obj.insert("services".to_string(), Value::Array(services));
}

/// Parse `systemctl list-units --no-legend` output into the heartbeat service
/// objects. Columns are `UNIT LOAD ACTIVE SUB DESCRIPTION`; the name is the unit
/// minus `.service` and the status is [`service_status`] of ACTIVE and SUB, in
/// the vocabulary the GCS renders (the cloud row carries only `name` and
/// `status`, so raw systemd words would all read as unknown). `list-units`
/// carries no per-unit accounting, so `uptimeSeconds`, `memoryMb` and
/// `cpuPercent` are omitted (the receiver declares them optional) rather than
/// asserted as a fabricated `0`. Pure for unit testing.
pub fn parse_systemctl_units(out: &str) -> Vec<Value> {
    let mut services = Vec::new();
    for line in out.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 4 {
            continue;
        }
        // A failed/dead unit gets a `●`/`*` lead glyph as its own column, which
        // shifts UNIT/LOAD/ACTIVE/SUB by one. Detect it by a column-0 token that
        // is not itself the `.service` unit, then index past it.
        let offset = if cols[0].ends_with(".service") { 0 } else { 1 };
        let unit = match cols.get(offset) {
            Some(u) => u.trim_start_matches(['●', '*']).trim(),
            None => continue,
        };
        if !unit.ends_with(".service") {
            continue;
        }
        // Columns from the unit: UNIT(+0) LOAD(+1) ACTIVE(+2) SUB(+3).
        let (Some(active), Some(sub)) = (cols.get(offset + 2), cols.get(offset + 3)) else {
            continue;
        };
        let name = &unit[..unit.len() - ".service".len()];
        services.push(json!({
            "name": name,
            "status": service_status(active.trim(), sub.trim()),
        }));
    }
    services
}

/// One unit's systemd ACTIVE + SUB state as a GCS service status (`running`,
/// `starting`, `stopped`, `error`, `degraded`). A unit systemd keeps restarting
/// is crash-looping (`error`) whatever its ACTIVE state reads between attempts;
/// an `active` oneshot that ran to completion (`exited`) did its job and is
/// `running`; an ACTIVE state not listed here is `degraded`, never a guessed
/// `stopped`.
pub fn service_status(active: &str, sub: &str) -> &'static str {
    if sub == "auto-restart" {
        return "error";
    }
    match active {
        "active" => "running",
        "activating" | "reloading" => "starting",
        "failed" => "error",
        "inactive" | "deactivating" => "stopped",
        _ => "degraded",
    }
}
