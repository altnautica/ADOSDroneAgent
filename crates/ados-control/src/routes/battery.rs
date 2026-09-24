//! `GET /api/v1/battery`: per-pack battery health.
//!
//! Always 200. The body carries the configured thresholds, whether the engine
//! is enabled, whether its data is stale (no snapshot sampled for 5 s: the
//! state hub is down or the FC link is lost), and one entry per pack with the
//! latest reading, the time-to-reserve projection, the live anomalies and the
//! transition history. Disabled, it serves no packs.

use std::time::SystemTime;

use axum::extract::State;
use axum::Json;
use serde_json::Value;

use crate::battery::epoch_ms;
use crate::state::AppState;

pub async fn get_battery(State(state): State<AppState>) -> Json<Value> {
    let now_ms = epoch_ms(SystemTime::now());
    let engine = state.battery.lock();
    // Plain structs with string keys: serialising to a `Value` cannot fail.
    Json(serde_json::to_value(engine.snapshot(now_ms)).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::battery::{BatteryConfig, BatteryEngine};
    use crate::ipc::{LogdQueryClient, MavlinkIpcClient, StateIpcClient};
    use crate::state::PairingPaths;

    /// An `AppState` whose only live part is the given battery engine.
    fn app_state(dir: &std::path::Path, engine: BatteryEngine) -> AppState {
        AppState::new(
            Arc::new(crate::auth::PairingState::with_path(
                dir.join("pairing.json"),
            )),
            StateIpcClient::disconnected(),
            MavlinkIpcClient::new(dir.join("absent-mavlink.sock")),
            LogdQueryClient::new(dir.join("absent-logd.sock")),
            dir.join("board.json"),
            PairingPaths {
                config: dir.join("config.yaml"),
                pairing_json: dir.join("pairing.json"),
                wfb_key_dir: dir.join("wfb"),
                bind_state: dir.join("bind-state.json"),
                profile_conf: dir.join("profile.conf"),
                mesh_role: dir.join("mesh-role"),
                relay_secret: dir.join("relay-secret"),
            },
            Arc::new(crate::dashboard_pin::DashboardPin::with_path(
                dir.join("dashboard-pin.json"),
            )),
            Arc::new(crate::mcp::McpTokenStore::with_path(dir.join("mcp.json"))),
        )
        .with_battery(Arc::new(parking_lot::Mutex::new(engine)))
    }

    fn keys(v: &Value) -> BTreeSet<&str> {
        v.as_object().unwrap().keys().map(String::as_str).collect()
    }

    /// The body is the contract Mission Control and the on-box dashboard parse:
    /// every documented key present, none extra.
    #[tokio::test]
    async fn the_body_carries_exactly_the_contract_keys() {
        let mut engine = BatteryEngine::new(BatteryConfig::default());
        let now = epoch_ms(SystemTime::now());
        engine.ingest(
            now,
            &json!({
                "mavlink_alive": true,
                "batteries": [{
                    "id": 0,
                    "cell_voltages": [3.9, 3.9, 3.9, 3.45],
                    "current_a": 12.0,
                    "remaining_pct": 60,
                    "temperature_c": 31.0,
                    "consumed_mah": 900,
                    "consumed_wh": 13.0,
                }],
            }),
        );
        let dir = tempfile::tempdir().unwrap();
        let Json(body) = get_battery(State(app_state(dir.path(), engine))).await;

        assert_eq!(
            keys(&body),
            BTreeSet::from(["enabled", "stale", "updated_at_ms", "thresholds", "packs"])
        );
        assert_eq!(
            keys(&body["thresholds"]),
            BTreeSet::from([
                "enabled",
                "low_cell_mv",
                "critical_cell_mv",
                "cell_divergence_mv",
                "voltage_drop_mv_per_s",
                "temp_spike_dc_per_s",
                "predictive_window_s",
                "reserve_percent",
            ])
        );
        let pack = &body["packs"][0];
        assert_eq!(
            keys(pack),
            BTreeSet::from([
                "id",
                "cells_plausible",
                "cell_voltages_v",
                "weakest_cell_index",
                "min_cell_v",
                "max_cell_v",
                "divergence_mv",
                "voltage_v",
                "current_a",
                "remaining_pct",
                "temperature_c",
                "consumed_mah",
                "consumed_wh",
                "prediction",
                "anomalies",
                "history",
            ])
        );
        assert_eq!(
            keys(&pack["prediction"]),
            BTreeSet::from(["state", "eta_s", "drop_pct_per_s", "mean_current_a"])
        );
        let anomaly = &pack["anomalies"][0];
        assert_eq!(
            keys(anomaly),
            BTreeSet::from([
                "rule",
                "severity",
                "value",
                "threshold",
                "first_seen_ms",
                "last_seen_ms",
                "cleared_at_ms",
            ])
        );
        assert_eq!(anomaly["rule"], json!("cell_low"));
        assert_eq!(anomaly["severity"], json!("warning"));
        assert_eq!(
            keys(&pack["history"][0]),
            BTreeSet::from(["rule", "severity", "state", "at_ms", "value"])
        );
        assert_eq!(pack["history"][0]["state"], json!("raised"));
        assert_eq!(body["stale"], json!(false));
    }

    #[tokio::test]
    async fn an_engine_with_no_samples_answers_stale_and_empty() {
        let dir = tempfile::tempdir().unwrap();
        let engine = BatteryEngine::new(BatteryConfig::default());
        let Json(body) = get_battery(State(app_state(dir.path(), engine))).await;
        assert_eq!(body["enabled"], json!(true));
        assert_eq!(body["stale"], json!(true));
        assert_eq!(body["updated_at_ms"], json!(0));
        assert_eq!(body["packs"], json!([]));
    }
}
