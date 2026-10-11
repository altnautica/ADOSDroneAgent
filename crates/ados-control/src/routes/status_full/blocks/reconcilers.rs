//! Reconciler status: management link, reach-back failover, USB rehome, WiFi power save.

use super::super::*;
use ados_protocol::paths;
use ados_protocol::sidecar::read_fresh_at;
use ados_protocol::sidecar::Reading;
use ados_protocol::wfb_status::get_or_null;
use serde_json::json;
use serde_json::Map;
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

/// The freshness window (seconds) for the management-link guardian sidecar: the
/// registry window for `mgmt-link`. The guardian ticks every few seconds inside
/// its fast-initial window and steadies after; the window tolerates the steady
/// cadence without ever rendering a stopped supervisor's last verdict as live.
pub(crate) const MGMT_LINK_FRESH_S: f64 = paths::MGMT_LINK_MAX_AGE_MS as f64 / 1000.0;

/// The freshness window (seconds) for the reach-back-mode sidecar: the registry
/// window for `mgmt-failover`. Same reasoning as [`MGMT_LINK_FRESH_S`]; both are
/// written by the same tick.
pub(crate) const MGMT_FAILOVER_FRESH_S: f64 = paths::MGMT_FAILOVER_MAX_AGE_MS as f64 / 1000.0;

/// The freshness window (seconds) for the USB-rehome sidecar: the registry
/// window for `usb-rehome`. It is written on a state change rather than a tick,
/// so the window is the generous one: an idle verdict that has not changed in
/// minutes is still the truth.
pub(crate) const USB_REHOME_FRESH_S: f64 = paths::USB_REHOME_MAX_AGE_MS as f64 / 1000.0;

/// Reconciler state for the LOCAL status surface: the management-link guardian,
/// the reach-back-mode failover, the USB-rehome self-heal, and the WiFi
/// power-save reconciler.
///
/// All four write a `/run/ados` sidecar every tick and, until this reader
/// existed, **nothing read any of them** — while the GCS carried a normalizer
/// clamp and a card for each one, permanently empty. Same defect as the camera
/// block above, four surfaces over. The keys are the camelCase ones
/// `agent-capabilities/normalizer.ts` already validates, so the GCS needs only to
/// fold them into its status-extras pick list.
///
/// Every block is omitted (not defaulted) when its sidecar is absent, stale, or
/// malformed: a stopped supervisor must read as "no reading", never as a healthy
/// verdict frozen at the moment it died.
pub(crate) fn read_reconciler_status(run_dir: &Path) -> Vec<(String, Value)> {
    read_reconciler_status_in(run_dir, now_unix_secs())
}

/// The path-injectable core of [`read_reconciler_status`]. Pure (no env reads), so
/// a test drives it with a tempdir without racing the process-global
/// `ADOS_RUN_DIR`.
pub(crate) fn read_reconciler_status_in(run_dir: &Path, now: f64) -> Vec<(String, Value)> {
    let mut out: Vec<(String, Value)> = Vec::new();

    // --- Management-link guardian → `managementLink` ---------------------------
    if let Some(ml) = read_versioned_sidecar(&run_dir.join("mgmt-link.json"), "mgmt-link") {
        if sidecar_fresh(&ml, now, MGMT_LINK_FRESH_S) {
            // The GCS clamp accepts the block only on a known state, so an
            // unrecognised verdict is dropped here rather than shipped.
            if let Some(state) = ml.get("state").and_then(Value::as_str) {
                if matches!(state, "healthy" | "degraded" | "down") {
                    out.push((
                        "managementLink".to_string(),
                        json!({
                            "state": state,
                            "iface": get_or_null(&ml, "iface"),
                            "transport": get_or_null(&ml, "transport"),
                            "backend": get_or_null(&ml, "backend"),
                            "carrier": json_truthy_default_false(&ml, "carrier"),
                            "hasLease": json_truthy_default_false(&ml, "has_lease"),
                            "gatewayReachable": json_truthy_default_false(&ml, "gateway_reachable"),
                            "repairing": json_truthy_default_false(&ml, "repairing"),
                            "lastRung": get_or_null(&ml, "last_rung"),
                            "lastRepairAt": get_or_null(&ml, "last_repair_at_unix"),
                            "repairsInWindow": ml.get("repairs_in_window").cloned().unwrap_or(json!(0)),
                        }),
                    ));
                }
            }
        }
    }

    // --- Reach-back mode → `mgmtLinkMode` + the two failover scalars -----------
    if let Some(mf) = read_versioned_sidecar(&run_dir.join("mgmt-failover.json"), "mgmt-failover") {
        if sidecar_fresh(&mf, now, MGMT_FAILOVER_FRESH_S) {
            if let Some(mode) = mf.get("mgmt_link_mode").and_then(Value::as_str) {
                if matches!(mode, "primary" | "wifi_heartbeat" | "none") {
                    out.push(("mgmtLinkMode".to_string(), json!(mode)));
                    out.push((
                        "mgmtFailoverIface".to_string(),
                        get_or_null(&mf, "mgmt_failover_iface"),
                    ));
                    out.push((
                        "mgmtFailoverReason".to_string(),
                        get_or_null(&mf, "mgmt_failover_reason"),
                    ));
                }
            }
        }
    }

    // --- USB rehome → `usbRehomeState` + the attempt scalars -------------------
    //
    // There is no `exhausted` state to report any more: the recovery machine no
    // longer has a terminal state, because the one it had could not clear
    // without the attempts it stopped. `usbRehomeAttempts` climbing with
    // `usbRehomeCooldownSeconds` fixed is what now tells an operator that a
    // radio has been failing for a while — a number that keeps moving, rather
    // than a state that says the vehicle gave up.
    if let Some(ur) = read_versioned_sidecar(&run_dir.join("usb-rehome.json"), "usb-rehome") {
        if sidecar_fresh(&ur, now, USB_REHOME_FRESH_S) {
            if let Some(state) = ur.get("usb_rehome_state").and_then(Value::as_str) {
                if matches!(state, "idle" | "rehoming" | "guard_blocked") {
                    out.push(("usbRehomeState".to_string(), json!(state)));
                    out.push((
                        "usbRehomeAttempts".to_string(),
                        ur.get("usb_rehome_attempts").cloned().unwrap_or(json!(0)),
                    ));
                    out.push((
                        "usbRehomeCooldownSeconds".to_string(),
                        ur.get("usb_rehome_cooldown_s").cloned().unwrap_or(json!(0)),
                    ));
                    out.push((
                        "usbRehomeLastResult".to_string(),
                        get_or_null(&ur, "usb_rehome_last_result"),
                    ));
                }
            }
        }
    }

    // --- WiFi power-save → `wifiPowersave.interfaces[]` ------------------------
    //
    // The only shape change of the four: the sidecar keys its per-interface
    // snapshots by interface NAME (a JSON object), while the GCS clamp requires
    // `interfaces` to be an ARRAY and each entry to carry its own `iface`. It also
    // carries no `updated_at_unix`, so freshness is the file's mtime — the same
    // gate the CRSF block applies for the same reason.
    let ps_path = run_dir.join(paths::WIFI_POWERSAVE_JSON);
    let reading = read_fresh_at::<Map<String, Value>>(
        &ps_path,
        Some(WIFI_POWERSAVE_FRESH),
        std::time::SystemTime::now(),
    );
    if let Reading::Fresh(ps) = reading {
        warn_on_version_drift(&ps, "wifi-powersave");
        if let Some(ifaces) = ps.get("interfaces").and_then(Value::as_object) {
            let rows: Vec<Value> = ifaces
                .iter()
                .filter_map(|(name, snap)| {
                    let s = snap.as_object()?;
                    Some(json!({
                        "iface": name,
                        "powersaveOn": json_truthy_default_false(s, "powersave_on"),
                        "reasserts": s.get("reasserts").cloned().unwrap_or(json!(0)),
                        "lastReassert": get_or_null(s, "last_reassert"),
                        "signalDbm": get_or_null(s, "signal_dbm"),
                        "linkState": s
                            .get("link_state")
                            .cloned()
                            .unwrap_or_else(|| json!("unknown")),
                    }))
                })
                .collect();
            // A node with no managed WiFi interface omits the block rather than
            // shipping an empty list, so the card stays hidden.
            if !rows.is_empty() {
                out.push(("wifiPowersave".to_string(), json!({"interfaces": rows})));
            }
        }
    }

    out
}

/// The freshness window for the WiFi power-save sidecar, gated on file mtime
/// because the writer stamps no in-body time: the registry window for
/// `wifi-powersave`.
pub(crate) const WIFI_POWERSAVE_FRESH: Duration =
    Duration::from_millis(paths::WIFI_POWERSAVE_MAX_AGE_MS);

/// Read a sidecar object and emit the best-effort schema-drift warning against the
/// shared contract registry. Never rejects on a version mismatch — a stale sidecar
/// from an older agent still reads.
pub(crate) fn read_versioned_sidecar(path: &Path, contract_id: &str) -> Option<Map<String, Value>> {
    let obj = read_sidecar_object(path)?;
    warn_on_version_drift(&obj, contract_id);
    Some(obj)
}

/// The best-effort schema-drift warning for a parsed sidecar object.
pub(crate) fn warn_on_version_drift(obj: &Map<String, Value>, contract_id: &str) {
    let got = obj.get("version").and_then(Value::as_u64).unwrap_or(0) as u16;
    if let Some(ours) = ados_protocol::contracts::sidecar_version(contract_id) {
        ados_protocol::sidecar::check_sidecar_version(contract_id, got, ours);
    }
}

/// Read + parse a sidecar file into an object, or `None` on any gap (absent /
/// read error / non-object body).
pub(crate) fn read_sidecar_object(path: &Path) -> Option<Map<String, Value>> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// How far ahead of `now` an in-body timestamp may sit and still count. Writer
/// and reader share one clock, so anything beyond rounding is a clock step and
/// the age is unprovable.
pub(crate) const SIDECAR_FUTURE_SKEW_S: f64 = 2.0;

/// True when a sidecar's `updated_at_unix` is a number within `max_age_s` of
/// `now`. A timestamp further than [`SIDECAR_FUTURE_SKEW_S`] in the future is
/// not fresh: after a backward clock step a dead writer's last file would
/// otherwise read as live until something overwrote it.
pub(crate) fn sidecar_fresh(obj: &Map<String, Value>, now: f64, max_age_s: f64) -> bool {
    match obj.get("updated_at_unix").and_then(Value::as_f64) {
        Some(updated) => (-SIDECAR_FUTURE_SKEW_S..=max_age_s).contains(&(now - updated)),
        None => false,
    }
}
