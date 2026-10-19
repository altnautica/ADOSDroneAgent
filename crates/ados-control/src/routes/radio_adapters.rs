//! `GET /api/v1/radio/adapters` — the WFB adapters the receive plane runs on.
//!
//! Serves `present` plus the `adapters` list and the `diversity` block of the
//! `wfb-stats` sidecar (version 2): per adapter its identity, role, antenna
//! label, whether it carries the transmit lanes, per-chain RSSI and its receive
//! counters, plus the combined-versus-best-single loss over the same window.
//! `present` is true only when the sidecar is fresh and was written by a
//! receive plane (the only writer of the per-adapter block). A sidecar that is
//! absent, stale past its registry window, or written by a producer without
//! the per-adapter block (the drone side) answers 200 with `present: false`
//! and the empty shape (`adapters: []`, `rx_adapters: 0`, null loss figures),
//! never a 404 and never a last-known reading dressed up as live.

use std::path::Path;
use std::time::{Duration, SystemTime};

use axum::extract::State;
use axum::Json;

use ados_protocol::paths;
use ados_protocol::radio_adapters::RadioAdaptersBody;
use ados_protocol::sidecar::{read_fresh_at, Reading};

use crate::state::AppState;

/// `GET /api/v1/radio/adapters` → `{present, adapters, diversity}`. Always 200.
pub async fn get_radio_adapters(State(state): State<AppState>) -> Json<RadioAdaptersBody> {
    Json(adapters_body_from(
        &state.run_dir.join(paths::WFB_STATS_JSON),
        SystemTime::now(),
    ))
}

/// The path-injectable core of [`get_radio_adapters`]: the sidecar's
/// `{adapters, diversity}` with `present: true` when it was written inside the
/// `wfb-stats` window by a receive plane, else the empty `present: false`
/// shape.
pub(crate) fn adapters_body_from(path: &Path, now: SystemTime) -> RadioAdaptersBody {
    let window = Some(Duration::from_millis(paths::WFB_STATS_MAX_AGE_MS));
    let Reading::Fresh(raw) = read_fresh_at::<serde_json::Value>(path, window, now) else {
        return RadioAdaptersBody::default();
    };
    // Only a receive plane writes the per-adapter block.
    if !raw.get("adapters").is_some_and(serde_json::Value::is_array) {
        return RadioAdaptersBody::default();
    }
    match serde_json::from_value::<RadioAdaptersBody>(raw) {
        Ok(body) => RadioAdaptersBody {
            present: true,
            ..body
        },
        Err(_) => RadioAdaptersBody::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sidecar() -> serde_json::Value {
        json!({
            "version": 2,
            "state": "active",
            "interface": "wlan1",
            "adapters": [
                {
                    "iface": "wlan1", "mac": "aa:bb:cc:dd:ee:01", "usb_path": "1-1.2",
                    "driver": "rtl88x2eu", "role": "rxtx", "antenna": "omni-5.8",
                    "tx_active": true, "rssi_dbm": [-48, -51], "packets": 120345,
                    "lost": 120, "fec_rec": null, "verified": true
                },
                {
                    "iface": "wlan2", "mac": "aa:bb:cc:dd:ee:02", "usb_path": "1-1.3",
                    "driver": "rtl88x2eu", "role": "rx", "antenna": null,
                    "tx_active": false, "rssi_dbm": [-60], "packets": 119000,
                    "lost": 1465, "fec_rec": null, "verified": true
                }
            ],
            "diversity": {"rx_adapters": 2, "combined_loss_pct": 0.1, "best_single_loss_pct": 1.2}
        })
    }

    fn empty() -> serde_json::Value {
        json!({
            "present": false,
            "adapters": [],
            "diversity": {"rx_adapters": 0, "combined_loss_pct": null, "best_single_loss_pct": null}
        })
    }

    #[test]
    fn a_fresh_receive_plane_sidecar_is_present_in_the_contract_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(paths::WFB_STATS_JSON);
        std::fs::write(&path, serde_json::to_vec(&sidecar()).unwrap()).unwrap();
        let body = serde_json::to_value(adapters_body_from(&path, SystemTime::now())).unwrap();
        let want = sidecar();
        assert_eq!(body["present"], true);
        assert_eq!(body["adapters"], want["adapters"]);
        assert_eq!(body["diversity"], want["diversity"]);
        // Only the three contract keys are served, not the whole sidecar.
        assert_eq!(body.as_object().unwrap().len(), 3);
    }

    #[test]
    fn a_receive_plane_with_no_adapter_yet_is_present_and_empty() {
        // A blocked receive plane (unpaired, no adapter) still writes the
        // per-adapter block, empty: the plane is there, it has nothing to list.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(paths::WFB_STATS_JSON);
        std::fs::write(
            &path,
            br#"{"version":2,"state":"no_adapter","adapters":[],"diversity":{"rx_adapters":0,"combined_loss_pct":null,"best_single_loss_pct":null}}"#,
        )
        .unwrap();
        let body = adapters_body_from(&path, SystemTime::now());
        assert!(body.present);
        assert!(body.adapters.is_empty());
    }

    #[test]
    fn an_absent_sidecar_is_the_empty_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(paths::WFB_STATS_JSON);
        let body = serde_json::to_value(adapters_body_from(&path, SystemTime::now())).unwrap();
        assert_eq!(body, empty());
    }

    #[test]
    fn a_stale_sidecar_is_the_empty_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(paths::WFB_STATS_JSON);
        std::fs::write(&path, serde_json::to_vec(&sidecar()).unwrap()).unwrap();
        // Read it as if well past the registry window.
        let later = SystemTime::now()
            + Duration::from_millis(paths::WFB_STATS_MAX_AGE_MS)
            + Duration::from_secs(5);
        let body = serde_json::to_value(adapters_body_from(&path, later)).unwrap();
        assert_eq!(body, empty());
    }

    #[test]
    fn a_sidecar_without_the_adapter_block_is_the_empty_shape() {
        // A version-1 producer (or the drone side) writes no per-adapter block.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(paths::WFB_STATS_JSON);
        std::fs::write(
            &path,
            br#"{"version":1,"state":"active","interface":"wlan0"}"#,
        )
        .unwrap();
        let body = serde_json::to_value(adapters_body_from(&path, SystemTime::now())).unwrap();
        assert_eq!(body, empty());
    }
}
