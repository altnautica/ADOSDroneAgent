use std::sync::Arc;
use std::time::Duration;

use ados_protocol::shutdown::Shutdown;
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use super::*;
use crate::sidecar::{build_stats_value, StatsInputs};
use crate::transport::{run_rx, WireCounters};
use ados_crsf_codec::frame::ADDR_FLIGHT_CONTROLLER;
use ados_crsf_codec::link::LaneState;
use ados_crsf_codec::telemetry::{build_telemetry_frame, Telemetry};

fn stats(rssi: (i8, i8), lq: u8, snr: i8, rf_mode: u8, tx_power: u8) -> LinkStatistics {
    LinkStatistics {
        uplink_rssi_ant1: rssi.0,
        uplink_rssi_ant2: rssi.1,
        uplink_lq: lq,
        uplink_snr: snr,
        active_antenna: 0,
        rf_mode,
        uplink_tx_power: tx_power,
        downlink_rssi: -70,
        downlink_lq: 90,
        downlink_snr: 4,
    }
}

fn module(id: &str, port: &str, role: ModuleRole) -> ModuleConfig {
    ModuleConfig {
        id: id.to_string(),
        port: port.to_string(),
        role,
        link: None,
    }
}

#[test]
fn rf_mode_maps_to_the_packet_rate_label() {
    let table = [
        (0, "4Hz"),
        (1, "25Hz"),
        (2, "50Hz"),
        (3, "100Hz"),
        (4, "100Hz Full"),
        (5, "150Hz"),
        (6, "200Hz"),
        (7, "250Hz"),
        (8, "333Hz Full"),
        (9, "500Hz"),
        (10, "D250"),
        (11, "D500"),
        (12, "F500"),
        (13, "F1000"),
        (14, "D50"),
    ];
    for (index, label) in table {
        assert_eq!(rf_mode_label(index), label, "rf_mode {index}");
    }
    // Outside the known table: the number, never a guessed rate.
    assert_eq!(rf_mode_label(15), "15");
    assert_eq!(rf_mode_label(255), "255");
}

#[test]
fn tx_power_index_maps_to_milliwatts() {
    let table = [
        (0, Some(0)),
        (1, Some(10)),
        (2, Some(25)),
        (3, Some(100)),
        (4, Some(500)),
        (5, Some(1000)),
        (6, Some(2000)),
        (7, Some(250)),
        (8, Some(50)),
        (9, None),
        (200, None),
    ];
    for (index, mw) in table {
        let link = ModuleLink::from_stats(&stats((-60, -62), 100, 8, 9, index));
        assert_eq!(link.tx_power_mw, mw, "tx power index {index}");
    }
}

#[test]
fn module_status_serialises_the_contract_shape() {
    let m = module("m0", "/dev/ttyACM1", ModuleRole::Pilot);
    let s = stats((-60, 62), 100, 8, 9, 7);
    let v = serde_json::to_value(ModuleStatus::new(&m, true, Some(&s))).unwrap();
    assert_eq!(
        v,
        json!({
            "id": "m0",
            "port": "/dev/ttyACM1",
            "role": "pilot",
            "link": {
                "rssi_dbm": [-60, -62],
                "lq": 100,
                "snr_db": 8,
                "tx_power_mw": 250,
                "rf_mode": "500Hz",
            },
            "connected": true,
            "domain": null,
        })
    );
    // No fresh link statistics: link is null, the module is still listed.
    let v = serde_json::to_value(ModuleStatus::new(&m, false, None)).unwrap();
    assert!(v["link"].is_null());
    assert_eq!(v["connected"], false);
}

/// Run one receiver over an in-memory stream, feed it one Link-Statistics
/// frame, and wait until the module's state holds it.
async fn feed_one(stats: LinkStatistics) -> Arc<Mutex<TelemetryState>> {
    let (mut writer, reader) = tokio::io::duplex(1024);
    let telemetry = Arc::new(Mutex::new(TelemetryState::default()));
    let cancel = Shutdown::new();
    let rx = tokio::spawn(run_rx(
        reader,
        telemetry.clone(),
        Arc::new(WireCounters::default()),
        cancel.clone(),
    ));
    let frame =
        build_telemetry_frame(ADDR_FLIGHT_CONTROLLER, &Telemetry::LinkStatistics(stats)).unwrap();
    writer.write_all(&frame).await.unwrap();
    writer.flush().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while telemetry.lock().await.last_link_stats.is_none() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("link statistics decoded within the deadline");
    cancel.trigger();
    let _ = rx.await;
    telemetry
}

#[tokio::test]
async fn sidecar_v2_carries_two_modules_with_independent_link_stats() {
    let pilot = module("m0", "/dev/ttyACM0", ModuleRole::Pilot);
    let autonomy = module("m1", "/dev/ttyACM1", ModuleRole::Autonomy);
    let pilot_state = feed_one(stats((-48, -51), 99, 9, 9, 3)).await;
    let autonomy_state = feed_one(stats((-80, -77), 61, -2, 2, 8)).await;

    let now = std::time::Instant::now();
    let pilot_link = fresh_link(&*pilot_state.lock().await, now);
    let autonomy_link = fresh_link(&*autonomy_state.lock().await, now);
    let modules = vec![
        ModuleStatus::new(&pilot, true, pilot_link.as_ref()),
        ModuleStatus::new(&autonomy, true, autonomy_link.as_ref()),
    ];
    let inputs = StatsInputs {
        link: pilot_link.as_ref(),
        modules: &modules,
        ..StatsInputs::default()
    };
    let v = build_stats_value(LaneState::LinkOk, &inputs);
    assert_eq!(v["v"], 2);
    // The top-level fields keep describing the pilot module.
    assert_eq!(v["lq_uplink"], 99);
    assert_eq!(
        v["modules"],
        json!([
            {
                "id": "m0", "port": "/dev/ttyACM0", "role": "pilot",
                "link": {"rssi_dbm": [-48, -51], "lq": 99, "snr_db": 9,
                         "tx_power_mw": 100, "rf_mode": "500Hz"},
                "connected": true, "domain": null,
            },
            {
                "id": "m1", "port": "/dev/ttyACM1", "role": "autonomy",
                "link": {"rssi_dbm": [-80, -77], "lq": 61, "snr_db": -2,
                         "tx_power_mw": 50, "rf_mode": "50Hz"},
                "connected": true, "domain": null,
            },
        ])
    );
}

#[test]
fn a_stale_link_is_not_reported() {
    let mut state = TelemetryState::default();
    let now = std::time::Instant::now();
    assert!(fresh_link(&state, now).is_none());
    let s = stats((-60, -62), 100, 8, 9, 7);
    state.last_link_stats = Some((s, now));
    assert_eq!(fresh_link(&state, now), Some(s));
    let later = now + STATS_FRESH_WINDOW + Duration::from_secs(1);
    assert!(fresh_link(&state, later).is_none());
}
