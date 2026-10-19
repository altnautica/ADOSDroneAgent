//! `GET /api/v1/radio/adapters` — the WFB adapters the receive plane runs on.
//!
//! Serves the `adapters` list and the `diversity` block of the `wfb-stats`
//! sidecar (version 2) verbatim: per adapter its identity, role, antenna label,
//! whether it carries the transmit lanes, per-chain RSSI and its receive
//! counters, plus the combined-versus-best-single loss over the same window.
//! A sidecar that is absent, stale past its registry window, or written by a
//! producer without the per-adapter block answers 200 with the empty shape
//! (`adapters: []`, `rx_adapters: 0`, null loss figures), never a 404 and never
//! a last-known reading dressed up as live.

use std::path::Path;
use std::time::{Duration, SystemTime};

use axum::extract::State;
use axum::Json;

use ados_protocol::paths;
use ados_protocol::radio_adapters::RadioAdaptersBody;
use ados_protocol::sidecar::{read_fresh_at, Reading};

use crate::state::AppState;

/// `GET /api/v1/radio/adapters` → `{adapters, diversity}`. Always 200.
pub async fn get_radio_adapters(State(state): State<AppState>) -> Json<RadioAdaptersBody> {
    Json(adapters_body_from(
        &state.run_dir.join(paths::WFB_STATS_JSON),
        SystemTime::now(),
    ))
}

/// The path-injectable core of [`get_radio_adapters`]: the sidecar's
/// `{adapters, diversity}` when it was written inside the `wfb-stats` window,
/// else the empty shape.
pub(crate) fn adapters_body_from(path: &Path, now: SystemTime) -> RadioAdaptersBody {
    let window = Some(Duration::from_millis(paths::WFB_STATS_MAX_AGE_MS));
    match read_fresh_at::<RadioAdaptersBody>(path, window, now) {
        Reading::Fresh(body) => body,
        Reading::Stale(_) | Reading::Absent | Reading::Malformed(_) => RadioAdaptersBody::default(),
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
            "adapters": [],
            "diversity": {"rx_adapters": 0, "combined_loss_pct": null, "best_single_loss_pct": null}
        })
    }

    #[test]
    fn a_fresh_sidecar_is_served_in_the_contract_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(paths::WFB_STATS_JSON);
        std::fs::write(&path, serde_json::to_vec(&sidecar()).unwrap()).unwrap();
        let body = serde_json::to_value(adapters_body_from(&path, SystemTime::now())).unwrap();
        let want = sidecar();
        assert_eq!(body["adapters"], want["adapters"]);
        assert_eq!(body["diversity"], want["diversity"]);
        // Only the two contract keys are served, not the whole sidecar.
        assert_eq!(body.as_object().unwrap().len(), 2);
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
