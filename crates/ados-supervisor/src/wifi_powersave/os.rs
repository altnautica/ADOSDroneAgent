//! OS edges for the WiFi power-save reconciler: the canonical-path config read,
//! the per-interface reconcile body, the sidecar writer, and the `iw` shells.
//!
//! All read/write edges are Linux-only; on a non-Linux dev host
//! `reconcile_wifi_powersave` is an inert no-op so the monitor-pass call site
//! stays portable across the dev host and CI. The pure parsers live in `parse`.
//! The sidecar struct + version + atomic writer are compiled on the test host too
//! so their round-trip and version discipline are unit-tested everywhere.

#[cfg(any(target_os = "linux", test))]
use ados_protocol::logd::emitter::EventEmitter;

#[cfg(target_os = "linux")]
use std::collections::{BTreeMap, HashMap};

#[cfg(target_os = "linux")]
use crate::config::CONFIG_YAML;

// The `iw` shells below run inside the monitor pass, so they route through the
// crate-wide bounded helpers: an unbounded `iw set` on a wedged driver would
// wedge service reconciliation for the whole process.
#[cfg(target_os = "linux")]
use crate::oscmd::{run_output, run_status};

#[cfg(target_os = "linux")]
use super::config::{read_config_from, WifiPowersaveConfig};
#[cfg(target_os = "linux")]
use super::{IfaceReassertState, WIFI_POWERSAVE_REASSERT_KIND};

/// The `/run/ados` sidecar the Python heartbeat reads to surface per-interface
/// power-save state.
#[cfg(target_os = "linux")]
const SIDECAR_PATH: &str = "/run/ados/wifi-powersave.json";

/// Schema version of the `wifi-powersave.json` sidecar. Bump on an incompatible
/// field-set change; kept in step with the registry in `contracts.toml`. Gated to
/// the platforms that build the writer (Linux) or the version test.
#[cfg(any(target_os = "linux", test))]
pub(super) const WIFI_POWERSAVE_SIDECAR_VERSION: u16 = 1;

/// Read `network.wifi_powersave` from the canonical config path. Re-read each
/// tick so a config edit takes effect without restarting the supervisor.
#[cfg(target_os = "linux")]
pub(super) fn read_config() -> WifiPowersaveConfig {
    match std::fs::read_to_string(CONFIG_YAML) {
        Ok(t) => read_config_from(&t),
        Err(_) => WifiPowersaveConfig::default(),
    }
}

/// Re-assert 802.11 power-save OFF on every station interface whose driver has
/// (re-)enabled it, verify the readback flipped, mirror the per-interface state
/// to the sidecar, and emit `wifi.powersave_reasserted` ONLY on a real re-assert
/// (a measured-on interface that confirmed off). A cheap no-op per interface when
/// power-save is already off. The persistent re-assert counters + last-reassert
/// timestamps live in `reasserts` across ticks; the sidecar reflects the running
/// totals.
#[cfg(target_os = "linux")]
pub(super) async fn reconcile_wifi_powersave(
    reasserts: &mut HashMap<String, IfaceReassertState>,
    events: &EventEmitter,
) {
    if !iw_available().await {
        return;
    }
    let Some(dev) = run_output("iw", &["dev"]).await else {
        return;
    };
    let ifaces: Vec<String> = super::parse::parse_station_interfaces(&dev)
        .into_iter()
        .filter(|iface| {
            let root = std::path::Path::new("/sys/class/net");
            let driver = ados_protocol::netif::driver_name_in(root, iface).unwrap_or_default();
            !ados_protocol::netif::is_wfb_adapter(&driver, ados_protocol::netif::usb_vid_pid(iface))
        })
        .collect();

    let mut snapshot: BTreeMap<String, WifiIfaceSnapshot> = BTreeMap::new();
    for iface in ifaces {
        // The measured power-save state before we touch anything.
        let measured = run_output("iw", &["dev", &iface, "get", "power_save"])
            .await
            .and_then(|o| super::parse::parse_power_save(&o));

        // `powersave_on` is the state AFTER the reconcile: unchanged when already
        // off / unreadable, flipped to off on a confirmed re-assert, or left as
        // the (still true) measured value when the set did not confirm.
        let mut powersave_on = measured.unwrap_or(false);

        if measured == Some(true) {
            let set_ok = run_status("iw", &["dev", &iface, "set", "power_save", "off"]).await;
            let after = if set_ok {
                run_output("iw", &["dev", &iface, "get", "power_save"])
                    .await
                    .and_then(|o| super::parse::parse_power_save(&o))
            } else {
                None
            };
            if after == Some(false) {
                powersave_on = false;
                let st = reasserts.entry(iface.clone()).or_default();
                st.count = st.count.saturating_add(1);
                st.last_reassert = Some(now_iso8601());
                tracing::info!(iface = %iface, "wifi_powersave_reasserted");
                events.emit(
                    WIFI_POWERSAVE_REASSERT_KIND,
                    ados_protocol::logd::Level::Info,
                    reassert_detail(&iface),
                );
            } else {
                // The set was issued but the readback did not confirm off. Report
                // the true (still-on) state rather than a state we did not verify.
                powersave_on = true;
                tracing::warn!(
                    iface = %iface,
                    note = "power-save set off but readback did not confirm",
                    "wifi_powersave_reassert_unconfirmed"
                );
            }
        }

        // The measured link signal + state for telemetry (never changed here).
        let link = run_output("iw", &["dev", &iface, "link"])
            .await
            .map(|o| super::parse::parse_link(&o))
            .unwrap_or_else(|| super::parse::LinkInfo {
                signal_dbm: None,
                link_state: "unknown".to_string(),
            });

        let st = reasserts.get(&iface);
        snapshot.insert(
            iface.clone(),
            WifiIfaceSnapshot {
                powersave_on,
                reasserts: st.map(|s| s.count).unwrap_or(0),
                last_reassert: st.and_then(|s| s.last_reassert.clone()),
                signal_dbm: link.signal_dbm,
                link_state: link.link_state,
            },
        );
    }

    let snap = WifiPowersaveSidecar {
        version: WIFI_POWERSAVE_SIDECAR_VERSION,
        interfaces: snapshot,
    };
    if let Err(e) = write_json_atomic(std::path::Path::new(SIDECAR_PATH), &snap, 0o644) {
        tracing::debug!(error = %e, "wifi_powersave sidecar write failed");
    }
}

/// Non-Linux test build: the reconcile has no OS edges to drive, so it is an
/// inert no-op that lets the reconcile-does-not-panic test run on the dev host.
/// (On a non-Linux, non-test build there is no caller — the non-Linux tick is a
/// bare no-op — so the body is only compiled under `test`.)
#[cfg(all(not(target_os = "linux"), test))]
pub(super) async fn reconcile_wifi_powersave(
    _reasserts: &mut std::collections::HashMap<String, super::IfaceReassertState>,
    _events: &EventEmitter,
) {
}

/// Build the `wifi.powersave_reasserted` detail map. Bland and reader-facing: it
/// names what the code did (turned power-save off on one station interface).
/// Built only on the Linux re-assert path (and in the detail test).
#[cfg(any(target_os = "linux", test))]
pub(super) fn reassert_detail(iface: &str) -> ados_protocol::logd::Fields {
    use ados_protocol::logd::{Fields, Value as MpVal};
    let mut d = Fields::new();
    d.insert("source".to_string(), MpVal::from("supervisor"));
    d.insert("iface".to_string(), MpVal::from(iface));
    d.insert("from".to_string(), MpVal::from("on"));
    d.insert("to".to_string(), MpVal::from("off"));
    d
}

// ---------------------------------------------------------------------------
// Sidecar (`/run/ados/wifi-powersave.json`)
// ---------------------------------------------------------------------------

/// The `/run/ados/wifi-powersave.json` snapshot. snake_case on disk; the Python
/// heartbeat maps it to a camelCase `wifiPowersave` object with an interfaces
/// array. The `interfaces` map is keyed by interface name and serializes as a
/// JSON object.
#[cfg(any(target_os = "linux", test))]
#[derive(serde::Serialize)]
struct WifiPowersaveSidecar {
    /// Sidecar schema version (best-effort drift signal for readers).
    version: u16,
    interfaces: std::collections::BTreeMap<String, WifiIfaceSnapshot>,
}

/// Per-interface power-save snapshot on disk.
#[cfg(any(target_os = "linux", test))]
#[derive(serde::Serialize)]
struct WifiIfaceSnapshot {
    /// The power-save state after this tick's reconcile.
    powersave_on: bool,
    /// Running total of on→off re-asserts this reconciler has made for the iface.
    reasserts: u64,
    /// ISO-8601 UTC timestamp of the last real re-assert, or null.
    last_reassert: Option<String>,
    /// The last measured RX signal in dBm, or null when unknown.
    signal_dbm: Option<i32>,
    /// The last measured link state (`connected` / `disconnected` / `unknown`).
    link_state: String,
}

/// Current UTC timestamp, `YYYY-MM-DDTHH:MM:SS+00:00` — the ISO-8601 seconds form
/// the agent's other sidecars use (matching the bind session's `iso_now`).
#[cfg(any(target_os = "linux", test))]
fn now_iso8601() -> String {
    use time::macros::format_description;
    const FMT: &[time::format_description::FormatItem<'_>] =
        format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]+00:00");
    time::OffsetDateTime::now_utc()
        .format(FMT)
        .unwrap_or_default()
}

#[cfg(any(target_os = "linux", test))]
use ados_protocol::sidecar::write_json_atomic;

// ---------------------------------------------------------------------------
// Linux OS edges
// ---------------------------------------------------------------------------

/// True when the `iw` binary is on PATH.
#[cfg(target_os = "linux")]
async fn iw_available() -> bool {
    crate::oscmd::binary_available("iw").await
}

#[cfg(test)]
mod tests;
