//! Per-adapter stats tests.

use super::*;
use ados_protocol::radio_adapters::AdapterRole;
use ados_radio::adapter::{resolve_roles, AdapterPlan, DetectedAdapter};
use ados_radio::config::RadioAdaptersConfig;

fn detected(i: u8) -> DetectedAdapter {
    DetectedAdapter {
        ifname: format!("wlan{i}"),
        mac: Some(format!("aa:bb:cc:dd:ee:0{i}")),
        usb_path: Some(format!("1-1.{i}")),
        driver: "rtl88x2eu".into(),
        chipset: "RTL8812EU".into(),
        verified: true,
        usb_speed_mbps: Some(480),
        usb_degraded: false,
    }
}

fn plan(n: u8, mesh: bool) -> AdapterPlan {
    let adapters: Vec<_> = (1..=n).map(detected).collect();
    AdapterPlan::new(resolve_roles(
        &adapters,
        &RadioAdaptersConfig::default(),
        mesh,
    ))
    .unwrap()
}

/// One interval from a two-adapter receiver: wlan_idx 0 (chains 0 and 1) and
/// wlan_idx 1 (chains 0 and 1), then the combined `PKT` line. The decoder saw
/// 95 unique + 5 lost = 100 frames; adapter 0 heard 90, adapter 1 heard 80.
const TWO_ADAPTER_INTERVAL: &[&str] = &[
    "1000\tRX_ANT\t5745:1:20\t0\t90:-52:-48:-44:20:25:30",
    "1000\tRX_ANT\t5745:1:20\t1\t88:-55:-51:-47:18:22:26",
    "1000\tRX_ANT\t5745:1:20\t100\t80:-64:-60:-56:10:14:18",
    "1000\tRX_ANT\t5745:1:20\t101\t78:-66:-62:-58:9:13:17",
    "1000\tPKT\t200:300000:0:1:95:95:7:5:0:95:280000",
];

#[test]
fn a_two_adapter_interval_splits_into_per_adapter_counters() {
    let mut t = AdapterStatsTracker::new(&plan(2, false));
    for line in TWO_ADAPTER_INTERVAL {
        t.feed_line(line);
    }
    let body = t.body();
    assert_eq!(body.adapters.len(), 2);
    let a = &body.adapters[0];
    assert_eq!(a.iface, "wlan1");
    assert_eq!(a.role, AdapterRole::Rxtx);
    assert!(a.tx_active);
    assert_eq!(a.rssi_dbm, vec![-48, -51]);
    assert_eq!(a.packets, 90);
    assert_eq!(a.lost, 10);
    // The decoder's FEC count is not attributable with two receivers.
    assert_eq!(a.fec_rec, None);
    let b = &body.adapters[1];
    assert_eq!(b.iface, "wlan2");
    assert_eq!(b.role, AdapterRole::Rx);
    assert!(!b.tx_active);
    assert_eq!(b.rssi_dbm, vec![-60, -62]);
    assert_eq!(b.packets, 80);
    assert_eq!(b.lost, 20);
    // Combined 5 % against the best single adapter's 10 %.
    assert_eq!(body.diversity.rx_adapters, 2);
    assert_eq!(body.diversity.combined_loss_pct, Some(5.0));
    assert_eq!(body.diversity.best_single_loss_pct, Some(10.0));
}

#[test]
fn counters_accumulate_and_rssi_is_the_last_interval() {
    let mut t = AdapterStatsTracker::new(&plan(2, false));
    for line in TWO_ADAPTER_INTERVAL {
        t.feed_line(line);
    }
    // Second interval: only adapter 0 hears anything.
    t.feed_line("2000\tRX_ANT\t5745:1:20\t0\t50:-50:-46:-42:20:25:30");
    t.feed_line("2000\tPKT\t50:150000:0:0:50:50:0:0:0:50:140000");
    let body = t.body();
    assert_eq!(body.adapters[0].packets, 140);
    assert_eq!(body.adapters[0].rssi_dbm, vec![-46]);
    assert_eq!(body.adapters[1].packets, 80);
    assert_eq!(body.adapters[1].lost, 70);
    assert!(body.adapters[1].rssi_dbm.is_empty());
    // Window: 150 expected, 5 lost combined, best single 10.
    assert_eq!(body.diversity.combined_loss_pct, Some(3.33));
    assert_eq!(body.diversity.best_single_loss_pct, Some(6.67));
}

#[test]
fn a_single_adapter_carries_the_decoder_counts_exactly() {
    let mut t = AdapterStatsTracker::new(&plan(1, false));
    t.feed_line("1000\tRX_ANT\t5745:1:20\t0\t98:-52:-48:-44:20:25:30");
    t.feed_line("1000\tPKT\t100:300000:0:1:95:95:7:5:0:95:280000");
    let body = t.body();
    let a = &body.adapters[0];
    assert_eq!(a.lost, 5);
    assert_eq!(a.fec_rec, Some(7));
    assert_eq!(body.diversity.combined_loss_pct, Some(5.0));
    assert_eq!(body.diversity.best_single_loss_pct, Some(5.0));
}

#[test]
fn the_mesh_adapter_is_reported_but_never_counted() {
    let mut t = AdapterStatsTracker::new(&plan(3, true));
    for line in TWO_ADAPTER_INTERVAL {
        t.feed_line(line);
    }
    let body = t.body();
    assert_eq!(body.adapters.len(), 3);
    let mesh = &body.adapters[2];
    assert_eq!(mesh.role, AdapterRole::Mesh);
    assert_eq!(mesh.packets, 0);
    assert!(mesh.rssi_dbm.is_empty());
    assert_eq!(mesh.fec_rec, None);
    assert_eq!(body.diversity.rx_adapters, 2);
}

#[test]
fn forwarded_antennas_and_unparsed_lines_are_ignored() {
    let mut t = AdapterStatsTracker::new(&plan(1, false));
    // An aggregator line from a relay at 10.0.0.5 (address in the high bits).
    t.feed_line("1000\tRX_ANT\t5745:1:20\ta00000500000000\t40:-50:-45:-40:20:25:30");
    t.feed_line("not a stats line");
    t.feed_line("1000\tPKT\t40:1000:0:0:40:40:0:0:0:40:900");
    assert_eq!(t.body().adapters[0].packets, 0);
}

#[test]
fn no_traffic_yet_reports_null_loss() {
    let t = AdapterStatsTracker::new(&plan(2, false));
    let v = serde_json::to_value(t.body()).unwrap();
    assert_eq!(v["diversity"]["rx_adapters"], 2);
    assert!(v["diversity"]["combined_loss_pct"].is_null());
    assert!(v["diversity"]["best_single_loss_pct"].is_null());
    assert_eq!(v["adapters"][0]["mac"], "aa:bb:cc:dd:ee:01");
    assert_eq!(v["adapters"][0]["usb_path"], "1-1.1");
    assert_eq!(v["adapters"][0]["verified"], true);
}

#[test]
fn the_body_lands_on_the_sidecar_payload() {
    let mut t = AdapterStatsTracker::new(&plan(2, false));
    for line in TWO_ADAPTER_INTERVAL {
        t.feed_line(line);
    }
    let mut payload = serde_json::json!({"version": 2, "interface": "wlan1"});
    t.insert_into(&mut payload);
    assert_eq!(payload["adapters"][1]["iface"], "wlan2");
    assert_eq!(payload["diversity"]["combined_loss_pct"], 5.0);
    assert_eq!(payload["interface"], "wlan1");
}
