//! Capability enrichment for the cloud heartbeat.
//!
//! The LAN route `/api/status/full` folds a set of camera, reconciler,
//! self-heal and display keys in from `/run/ados` sidecars. The cloud
//! heartbeat — the only producer since the Python assembler was removed —
//! folded none of them, so the same node was a different product depending on
//! transport: over the relay the adapter-stability, Wi-Fi power-save,
//! self-heal and management-link cards rendered empty, the camera-missing
//! badge and the USB-recovery banner never fired, and `capability:camera`
//! reported missing on a drone with a working camera.
//!
//! This reads the same sidecars and emits the same camelCase keys the GCS
//! capability normalizer already validates, so the fold is additive on the
//! wire and needs no GCS-side shape work.
//!
//! # Absent is absent
//!
//! Every block is omitted — not defaulted — when its sidecar is missing,
//! stale, or malformed. A stopped reconciler must read as "no reading", never
//! as a healthy verdict frozen at the moment it died. `build_payload`'s
//! null-strip removes anything that slips through as null, and the GCS treats
//! an absent key as unknown rather than as false.
//!
//! # What is deliberately not here
//!
//! `visionActiveModel`, `visionBackend`, `radioStackState`, `setupState`,
//! `profileSource`, `wfbFailoverState` and `videoRestartAttempts` have no
//! producer anywhere in the agent — not on this loop and not on the LAN status
//! route either. Emitting a value for them would be inventing a reading, so
//! they stay absent until something measures them.

use ados_protocol::paths::{self, run_dir};
use serde_json::{json, Map, Value};
use std::path::Path;

/// Freshness windows: the registry windows the LAN route's gates read too, so
/// the two transports agree on when a reading has expired.
const CAMERA_STATE_FRESH_S: f64 = paths::CAMERA_STATE_MAX_AGE_MS as f64 / 1000.0;
const CAMERA_RECOVERY_FRESH_S: f64 = paths::CAMERA_USB_RECOVERY_MAX_AGE_MS as f64 / 1000.0;
const MGMT_LINK_FRESH_S: f64 = paths::MGMT_LINK_MAX_AGE_MS as f64 / 1000.0;
const MGMT_FAILOVER_FRESH_S: f64 = paths::MGMT_FAILOVER_MAX_AGE_MS as f64 / 1000.0;
const USB_REHOME_FRESH_S: f64 = paths::USB_REHOME_MAX_AGE_MS as f64 / 1000.0;
const WIFI_POWERSAVE_FRESH: std::time::Duration =
    std::time::Duration::from_millis(paths::WIFI_POWERSAVE_MAX_AGE_MS);

fn now_unix_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn read_object(path: &Path) -> Option<Map<String, Value>> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<Value>(&text)
        .ok()?
        .as_object()
        .cloned()
}

/// Whether a sidecar's own `updated_at_unix` is inside `window`. A file with
/// no stamp is treated as fresh: the writer chose not to date it, and the
/// alternative is dropping a reading that may be current.
fn fresh(obj: &Map<String, Value>, now: f64, window: f64) -> bool {
    match obj.get("updated_at_unix").and_then(Value::as_f64) {
        Some(at) => now - at <= window,
        None => true,
    }
}

/// Whether the file's mtime is within `window` of now. A future mtime (a clock
/// that stepped backwards) is an unprovable age and reads as not fresh.
fn mtime_fresh(path: &Path, window: std::time::Duration) -> bool {
    ados_protocol::sidecar::file_age(path, std::time::SystemTime::now())
        .is_some_and(|age| age <= window)
}

fn or_null(obj: &Map<String, Value>, key: &str) -> Value {
    obj.get(key).cloned().unwrap_or(Value::Null)
}

fn truthy(obj: &Map<String, Value>, key: &str) -> Value {
    json!(obj.get(key).and_then(Value::as_bool).unwrap_or(false))
}

/// Every capability key this loop can observe, as the heartbeat's camelCase
/// root keys. Folded over the typed base by `build_payload`.
pub fn capability_extras() -> Map<String, Value> {
    capability_extras_in(&run_dir(), now_unix_secs())
}

/// The path-injectable core. Pure over `(dir, now)`, so a test drives it with
/// a tempdir instead of racing the process-global run directory.
pub fn capability_extras_in(dir: &Path, now: f64) -> Map<String, Value> {
    let mut out = Map::new();

    // --- Camera presence + USB recovery ------------------------------------
    // Discovery alone is not health: a detected camera behind a failed
    // pipeline is an error, not a confident "ready".
    if let Some(state) = ados_video::camera_state::read_effective_state(
        &dir.join("camera-state.json"),
        now,
        CAMERA_STATE_FRESH_S,
    ) {
        out.insert("cameraState".into(), json!(state.as_str()));
    }
    if let Some(rec) = read_object(&dir.join("camera-usb-recovery.json")) {
        if fresh(&rec, now, CAMERA_RECOVERY_FRESH_S) {
            if let Some(state) = rec.get("camera_usb_recovery_state").and_then(Value::as_str) {
                if matches!(
                    state,
                    "idle"
                        | "monitoring"
                        | "rebinding"
                        | "port_cycling"
                        | "hub_resetting"
                        | "needs_hub_reset"
                        | "guard_blocked"
                        | "retrying"
                ) {
                    out.insert(
                        "cameraUsbRecovery".into(),
                        json!({
                            "state": state,
                            "case": or_null(&rec, "case"),
                            "attempts": rec.get("attempts").cloned().unwrap_or(json!(0)),
                            "cooldownSeconds": rec.get("cooldown_s").cloned().unwrap_or(json!(0)),
                            "cameraPresent": truthy(&rec, "camera_present"),
                            "expected": truthy(&rec, "expected"),
                            "pppsCapable": truthy(&rec, "ppps_capable"),
                            "powerContention": truthy(&rec, "power_contention"),
                            "contentionPeer": or_null(&rec, "contention_peer"),
                        }),
                    );
                }
            }
        }
    }

    // --- Management-link guardian ------------------------------------------
    if let Some(ml) = read_object(&dir.join("mgmt-link.json")) {
        if fresh(&ml, now, MGMT_LINK_FRESH_S) {
            if let Some(state) = ml.get("state").and_then(Value::as_str) {
                if matches!(state, "healthy" | "degraded" | "down") {
                    out.insert(
                        "managementLink".into(),
                        json!({
                            "state": state,
                            "iface": or_null(&ml, "iface"),
                            "transport": or_null(&ml, "transport"),
                            "backend": or_null(&ml, "backend"),
                            "carrier": truthy(&ml, "carrier"),
                            "hasLease": truthy(&ml, "has_lease"),
                            "gatewayReachable": truthy(&ml, "gateway_reachable"),
                            "repairing": truthy(&ml, "repairing"),
                            "lastRung": or_null(&ml, "last_rung"),
                            "lastRepairAt": or_null(&ml, "last_repair_at_unix"),
                            "repairsInWindow": ml
                                .get("repairs_in_window")
                                .cloned()
                                .unwrap_or(json!(0)),
                        }),
                    );
                }
            }
        }
    }

    // --- Reach-back mode ----------------------------------------------------
    if let Some(mf) = read_object(&dir.join("mgmt-failover.json")) {
        if fresh(&mf, now, MGMT_FAILOVER_FRESH_S) {
            if let Some(mode) = mf.get("mgmt_link_mode").and_then(Value::as_str) {
                if matches!(mode, "primary" | "wifi_heartbeat" | "none") {
                    out.insert("mgmtLinkMode".into(), json!(mode));
                    out.insert(
                        "mgmtFailoverIface".into(),
                        or_null(&mf, "mgmt_failover_iface"),
                    );
                    out.insert(
                        "mgmtFailoverReason".into(),
                        or_null(&mf, "mgmt_failover_reason"),
                    );
                }
            }
        }
    }

    // --- USB rehome self-heal -----------------------------------------------
    if let Some(ur) = read_object(&dir.join("usb-rehome.json")) {
        if fresh(&ur, now, USB_REHOME_FRESH_S) {
            if let Some(state) = ur.get("usb_rehome_state").and_then(Value::as_str) {
                if matches!(state, "idle" | "rehoming" | "guard_blocked") {
                    out.insert("usbRehomeState".into(), json!(state));
                    out.insert(
                        "usbRehomeAttempts".into(),
                        ur.get("usb_rehome_attempts").cloned().unwrap_or(json!(0)),
                    );
                    out.insert(
                        "usbRehomeCooldownSeconds".into(),
                        ur.get("usb_rehome_cooldown_s").cloned().unwrap_or(json!(0)),
                    );
                    out.insert(
                        "usbRehomeLastResult".into(),
                        or_null(&ur, "usb_rehome_last_result"),
                    );
                }
            }
        }
    }

    // --- Wi-Fi power-save ---------------------------------------------------
    //
    // The one shape change: the sidecar keys its snapshots by interface NAME
    // while the GCS clamp requires an ARRAY whose entries carry their own
    // `iface`. It also carries no in-body stamp, so freshness is the mtime.
    let ps_path = dir.join("wifi-powersave.json");
    if mtime_fresh(&ps_path, WIFI_POWERSAVE_FRESH) {
        if let Some(ps) = read_object(&ps_path) {
            if let Some(ifaces) = ps.get("interfaces").and_then(Value::as_object) {
                let rows: Vec<Value> = ifaces
                    .iter()
                    .filter_map(|(name, snap)| {
                        let s = snap.as_object()?;
                        Some(json!({
                            "iface": name,
                            "powersaveOn": truthy(s, "powersave_on"),
                            "reasserts": s.get("reasserts").cloned().unwrap_or(json!(0)),
                            "lastReassert": or_null(s, "last_reassert"),
                            "signalDbm": or_null(s, "signal_dbm"),
                            "linkState": s
                                .get("link_state")
                                .cloned()
                                .unwrap_or_else(|| json!("unknown")),
                        }))
                    })
                    .collect();
                // A node with no managed Wi-Fi interface omits the block rather
                // than shipping an empty list, so the card stays hidden.
                if !rows.is_empty() {
                    out.insert("wifiPowersave".into(), json!({ "interfaces": rows }));
                }
            }
        }
    }

    // --- LCD page -----------------------------------------------------------
    if let Some(lcd) = read_object(&dir.join("lcd-state.json")) {
        if let Some(page) = lcd.get("active_page_id").and_then(Value::as_str) {
            if !page.is_empty() {
                out.insert("lcdActivePage".into(), json!(page));
            }
        }
    }

    out
}

#[cfg(test)]
mod tests;
