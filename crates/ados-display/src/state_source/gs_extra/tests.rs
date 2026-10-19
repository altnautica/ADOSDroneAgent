use serde_json::json;

use super::*;
use crate::pages::gs_ctx::LoraTxState;

const NOW_MS: u64 = 1_760_860_012_000;

fn lora_in() -> Value {
    json!({"present":true,"transport":"e22","module_band":"850-930",
     "region":{"code":"IN","name":"India","plan_id":"IN865","source":"network.regulatory.region","band_mhz":[865.0,867.0],"citation":"G.S.R. 853(E), 2021"},
     "channel":{"freq_mhz":866.0,"bw_khz":125,"sf":9,"erp_mw":25,"max_erp_mw":25},
     "node_id":17,"tx_allowed":true,"blocked_reason":null,
     "governor":{"rules":[
       {"kind":"duty_cycle","window_s":3600,"limit_pct":1.0,"used_pct":0.4,"airtime_ms_window":14400,"budget_ms_window":36000,"exhausted":false,"sub_band":"865.0-867.0"},
       {"kind":"dwell_time","limit_ms":400,"per_s":20,"last_tx_ms":206,"ok":true},
       {"kind":"lbt","threshold_dbm":-80,"listen_us":5000,"last_rssi_dbm":-112,"busy_count":0},
       {"kind":"max_power","limit_mw":25,"configured_mw":25,"ok":true}],
      "next_tx_allowed_ms":0,"last_refusal":null},
     "tx":{"frames":120,"refused":0,"refused_by_rule":{"duty_cycle":0,"dwell_time":0,"lbt":0,"max_power":0,"region_unset":0}},
     "rx":{"frames":340,"crc_errors":2},
     "meshtastic":{"enabled":false,"connected":false,"port":null,"region_preset":"IN"}})
}

fn beacons() -> Value {
    json!({"beacons":[
        {"node_id":42,"kind":"beacon","lat_deg":12.97,"lon_deg":77.59,"alt_m":40.0,"battery_pct":81,"mode":"AUTO","rssi_dbm":-97,"snr_db":6.5,"received_at_ms":1_760_860_000_000u64},
        {"node_id":7,"kind":"beacon","lat_deg":12.97,"lon_deg":77.59,"alt_m":38.0,"battery_pct":64,"mode":"LOITER","rssi_dbm":-101,"snr_db":2.0,"received_at_ms":1_760_859_971_000u64},
        {"node_id":9,"kind":"beacon","lat_deg":12.97,"lon_deg":77.59,"alt_m":38.0,"battery_pct":60,"mode":"AUTO","rssi_dbm":-110,"snr_db":-3.0,"received_at_ms":1_760_859_000_000u64}]})
}

#[test]
fn lora_normal_in_plan() {
    let ctx = lora_ctx(Some(&lora_in()), Some(&beacons()), NOW_MS).unwrap();
    assert_eq!(ctx.region_label(), "IN");
    assert_eq!(ctx.plan_id.as_deref(), Some("IN865"));
    assert_eq!(ctx.tx_state(), LoraTxState::Ok);
    assert_eq!(ctx.duty_rules.len(), 1);
    let duty = ctx.top_duty().unwrap();
    assert_eq!((duty.used_pct, duty.limit_pct), (0.4, 1.0));
    assert_eq!(ctx.erp_mw, Some(25.0));
    assert_eq!(ctx.refused, Some(0));
    assert_eq!(ctx.beacons_heard, 3);
    assert_eq!(ctx.beacons.len(), 2);
    assert_eq!(ctx.beacons[0].node_id, 42);
    assert_eq!(ctx.beacons[0].age_s, Some(12));
    assert_eq!(ctx.beacons[1].age_s, Some(41));
}

#[test]
fn lora_region_unset() {
    let mut v = lora_in();
    v["region"] = Value::Null;
    v["tx_allowed"] = json!(false);
    v["blocked_reason"] = json!("region_unset");
    v["governor"]["rules"] = json!([]);
    let ctx = lora_ctx(Some(&v), None, NOW_MS).unwrap();
    assert_eq!(ctx.region_label(), "--");
    assert_eq!(ctx.tx_state(), LoraTxState::BlockedRegion);
    assert_eq!(ctx.tx_state().short(), "TX BLK REGION");
    assert!(ctx.top_duty().is_none());
    assert!(ctx.beacons.is_empty());
}

#[test]
fn lora_duty_exhausted_counts_down() {
    let mut v = lora_in();
    v["tx_allowed"] = json!(false);
    v["blocked_reason"] = json!("duty_exhausted");
    v["governor"]["rules"][0]["used_pct"] = json!(1.0);
    v["governor"]["next_tx_allowed_ms"] = json!(462_000);
    let ctx = lora_ctx(Some(&v), None, NOW_MS).unwrap();
    assert_eq!(ctx.tx_state().short(), "TX WAIT 07:42");
    assert_eq!(
        ctx.top_duty().unwrap().level(),
        Some(crate::pages::gs_ctx::Level::Error)
    );
}

#[test]
fn lora_module_band_mismatch() {
    let mut v = lora_in();
    v["tx_allowed"] = json!(false);
    v["blocked_reason"] = json!("module_band_mismatch");
    v["region"]["code"] = json!("CN");
    let ctx = lora_ctx(Some(&v), None, NOW_MS).unwrap();
    assert_eq!(ctx.tx_state().short(), "TX BLK BAND");
    assert_eq!(ctx.region_label(), "CN");
}

#[test]
fn absent_services_read_none() {
    let absent = json!({"present": false});
    assert!(lora_ctx(Some(&absent), None, NOW_MS).is_none());
    assert!(lora_ctx(None, None, NOW_MS).is_none());
    assert!(gnss_ctx(Some(&absent)).is_none());
    assert!(gnss_ctx(None).is_none());
    let no_rx = json!({"present":false,"adapters":[],
        "diversity":{"rx_adapters":0,"combined_loss_pct":null,"best_single_loss_pct":null}});
    assert!(radio_adapters_ctx(Some(&no_rx)).is_none());
    assert!(radio_adapters_ctx(None).is_none());
}

#[test]
fn gnss_maps_first_receiver_base_and_heading() {
    let v = json!({"present":true,"receivers":[{"id":"gnss0","source":"hub","model":"u-blox M10","fix":"3d","sats":14,"hdop":0.8,"lat_deg":12.9716,"lon_deg":77.5946,"h_ellipsoid_m":920.4,"h_msl_m":1006.1,"rtk":"none","pps_locked":true}],
     "base":{"mode":"survey","state":"surveying","elapsed_s":120,"min_dur_s":300,"accuracy_m":3.1,"target_accuracy_m":2.0,"fixed":null,"rtcm_out":{"msgs_per_s":0.0,"bytes_total":0,"targets":0}},
     "heading":{"available":false,"deg":null,"accuracy_deg":null,"baseline_m":null}});
    let g = gnss_ctx(Some(&v)).unwrap();
    assert_eq!(g.fix.as_deref(), Some("3d"));
    assert_eq!(g.sats, Some(14));
    assert_eq!(g.hdop, Some(0.8));
    assert_eq!(g.pps_locked, Some(true));
    let base = g.base.unwrap();
    assert_eq!(base.mode.as_deref(), Some("survey"));
    assert_eq!(base.survey_fraction(), Some(0.4));
    assert_eq!(g.heading_deg, None);

    let mut v2 = v.clone();
    v2["heading"] = json!({"available":true,"deg":274.3,"accuracy_deg":0.5,"baseline_m":1.0});
    assert_eq!(gnss_ctx(Some(&v2)).unwrap().heading_deg, Some(274.3));
}

#[test]
fn radio_adapters_decode_the_protocol_body() {
    let v = json!({"present":true,"adapters":[{"iface":"wlan1","mac":"aa:bb:cc:dd:ee:01","usb_path":"1-1.2","driver":"rtl88x2eu","role":"rxtx","antenna":"omni-5.8","tx_active":true,
      "rssi_dbm":[-48,-51],"packets":120345,"lost":120,"fec_rec":540,"verified":true}],
     "diversity":{"rx_adapters":3,"combined_loss_pct":0.1,"best_single_loss_pct":1.2}});
    let r = radio_adapters_ctx(Some(&v)).unwrap();
    assert_eq!(r.adapters.len(), 1);
    assert_eq!(crate::pages::gs_ctx::best_rssi(&r.adapters[0]), Some(-48));
    assert_eq!(r.diversity.rx_adapters, 3);
}
