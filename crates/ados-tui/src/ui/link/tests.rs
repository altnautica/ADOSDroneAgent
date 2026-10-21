use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::app::{App, Slot, Tab, TabId};
use crate::model::ProfileRoutes;
use crate::poll::ResourceId;
use crate::ui::tests::{draw, feed, test_app};

fn land(app: &mut App, id: ResourceId, value: Value) {
    app.core.res.insert(
        id.clone(),
        Slot {
            value: Some(value),
            error: None,
            code: None,
            error_body: None,
            at: Instant::now(),
        },
    );
    app.tabs.link.on_resource(&mut app.core, &id);
}

fn app_for(profile: &str) -> App {
    let mut app = test_app();
    let status = json!({
        "version": "0.99.108", "device_name": "ados-x", "profile": profile,
        "paired": true, "mavlink": {"connected": false}
    });
    feed(&mut app, status, ProfileRoutes::default(), Duration::ZERO);
    app.activate_tab(TabId::Link);
    app
}

fn wfb(fec_recovered: u64) -> Value {
    json!({
        "state": "connected", "interface": "wlan1", "channel": 149, "actual_channel": 149,
        "frequency_mhz": 5745, "bandwidth_mhz": 20, "adapter": {"driver": "", "chipset": ""},
        "adapter_chipset": "RTL8812AU", "rssi_dbm": -52.0, "snr_db": 28.0, "noise_dbm": -80.0,
        "packets_received": 180, "loss_percent": 2.5, "fec_recovered": fec_recovered, "fec_failed": 0,
        "bitrate_kbps": 2000, "restart_count": 0, "tx_power_dbm": 5, "tx_power_max_dbm": 15,
        "mcs_index": 1, "regulatory_domain": "US", "link_diag": "healthy", "profile": "drone"
    })
}

#[test]
fn link_renders_the_wfb_card_trends_and_lora() {
    let mut app = app_for("drone");
    land(&mut app, ResourceId::Wfb, wfb(5));
    land(&mut app, ResourceId::Wfb, wfb(7));
    land(
        &mut app,
        ResourceId::Lora,
        json!({
            "present": true,
            "region": {"code": "IN", "name": "India", "band_mhz": [865.0, 867.0]},
            "channel": {"freq_mhz": 865.0625, "bw_khz": 125, "sf": 9, "erp_mw": 25.0, "max_erp_mw": 25.0},
            "tx_allowed": false, "blocked_reason": "duty_exhausted",
            "governor": {"rules": [{"kind": "duty_cycle", "limit_pct": 1.0, "used_pct": 0.123, "sub_band": "865.0-867.0"}]},
            "tx": {"frames": 10, "refused": 0}, "rx": {"frames": 4, "crc_errors": 0}
        }),
    );
    land(
        &mut app,
        ResourceId::LoraBeacons,
        json!({"beacons": [
            {"node_id": 7, "mode": "AUTO", "battery_pct": 81, "rssi_dbm": -95, "snr_db": 7.5, "received_at_ms": 0}
        ]}),
    );
    let text = draw(&mut app, 120, 40);
    for want in [
        "WFB radio",
        "CONNECTED",
        "wlan1 · RTL8812AU",
        "149 · 5745 MHz · 20 MHz wide",
        "MCS 1 · 5 of 15 dBm",
        "RSSI -52 dBm · SNR 28 dB",
        "loss 2.5 %",
        "recovered 7",
        "RSSI dBm · SNR dB",
        "Bitrate 2.00 Mbit/s",
        "LoRa",
        "IN India",
        "blocked: duty exhausted",
        "0.12 % of 1 %",
        "AUTO",
    ] {
        assert!(text.contains(want), "missing {want:?}\n{text}");
    }
}

#[test]
fn ground_station_shows_adapter_deltas_receiver_and_mesh() {
    let mut app = app_for("ground_station");
    let gs = |packets: u64| {
        json!({"state": "active", "interface": "wlan1", "profile": "ground_station",
               "adapters": [{"iface": "wlan1", "role": "rxtx", "rssi_dbm": [-50, -52],
                             "packets": packets, "lost": 3, "fec_rec": null}]})
    };
    land(&mut app, ResourceId::Wfb, gs(1000));
    land(&mut app, ResourceId::Wfb, gs(1250));
    land(
        &mut app,
        ResourceId::ReceiverCombined,
        json!({"fragments_after_dedup": 100, "fec_repaired": 2, "output_kbps": 4200, "up": true, "stale": false}),
    );
    land(
        &mut app,
        ResourceId::ReceiverCombined,
        json!({"fragments_after_dedup": 160, "fec_repaired": 5, "output_kbps": 4200, "up": true, "stale": false}),
    );
    land(
        &mut app,
        ResourceId::ReceiverRelays,
        json!({"relays": [], "stale": false}),
    );
    land(
        &mut app,
        ResourceId::Mesh,
        json!({
            "role": "receiver", "mesh_id": "ados-abc", "up": true, "partition": false,
            "neighbors": [{"mac": "aa:bb:cc:dd:ee:ff", "iface": "wlan1", "tq": 255, "last_seen_ms": 0}]
        }),
    );
    let text = draw(&mut app, 120, 40);
    for want in [
        "Adapters",
        "-50/-52",
        "+250",
        "+60",
        "+3",
        "4200 kbit/s",
        "aa:bb:cc:dd:ee:ff",
        "255",
    ] {
        assert!(text.contains(want), "missing {want:?}\n{text}");
    }
}

#[test]
fn a_counter_that_went_backwards_shows_no_delta() {
    let mut app = app_for("ground_station");
    let gs = |packets: u64| {
        json!({"state": "active", "profile": "ground_station",
               "adapters": [{"iface": "wlan1", "role": "rx", "packets": packets, "lost": 0}]})
    };
    land(&mut app, ResourceId::Wfb, gs(90_000));
    land(&mut app, ResourceId::Wfb, gs(12));
    let text = draw(&mut app, 120, 40);
    assert!(!text.contains("+18446744"), "{text}");
    assert!(!text.contains("+89988"), "{text}");
}

#[test]
fn leaving_the_tab_resets_counter_baselines() {
    let mut app = app_for("ground_station");
    let gs = |packets: u64| {
        json!({"state": "active", "profile": "ground_station",
               "adapters": [{"iface": "wlan1", "role": "rx", "packets": packets, "lost": 0}]})
    };
    land(&mut app, ResourceId::Wfb, gs(1000));
    land(&mut app, ResourceId::Wfb, gs(1250));
    assert!(draw(&mut app, 120, 40).contains("+250"));

    app.tabs.link.on_deactivate(&mut app.core);
    land(&mut app, ResourceId::Wfb, gs(9000));
    let text = draw(&mut app, 120, 40);
    assert!(!text.contains("+7750"), "the gap must not read as one interval\n{text}");
    land(&mut app, ResourceId::Wfb, gs(9100));
    assert!(draw(&mut app, 120, 40).contains("+100"));
}
