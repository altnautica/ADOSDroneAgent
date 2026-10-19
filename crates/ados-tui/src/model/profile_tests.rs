use super::*;
use serde_json::json;

pub(crate) fn sensors_fixture(hub: bool, fc: bool) -> Value {
    json!({"present":true,"rig_id":"visor-pro-01","calib_version":"2026-10-19a",
     "hub":{"connected":hub,"transport":"serial:/dev/ttyACM0","fw":"ados-sensorhub 0.1.0","board":"matek_h743_slim_v3","proto_ver":1,"uptime_s":321,"crc_errors":0,"dropped":0},
     "fc":{"connected":fc,"autopilot":"ardupilot","highres_imu":true},
     "sensors":[{"id":"imu0","kind":"imu","role":"nav","source":"hub","rate_hz":1000.0,"expected_hz":1000.0,"last_t_ns":123456789000_u64,"age_ms":1.0,"sigma_ns":420000},
                {"id":"cam_fl","kind":"camera","role":"front_left","source":"vision","rate_hz":12.0,"expected_hz":20.0,"last_t_ns":123456780000_u64,"age_ms":9.0,"sigma_ns":420000},
                {"id":"gnss0","kind":"gnss","role":"nav","source":"hub","rate_hz":10.0,"expected_hz":null,"last_t_ns":0,"age_ms":null,"sigma_ns":-1}],
     "gnss":{"fix":"3d","sats":14,"hdop":0.8,"rtk":"none"}})
}

pub(crate) fn visor_routes(hub: bool, fc: bool) -> ProfileRoutes {
    ProfileRoutes {
        sensors: Some(sensors_fixture(hub, fc)),
        clock: Some(
            json!({"present":true,"timebase":"host_monotonic","locked":true,"sigma_ns":40000,"t_ns":123456789000_u64,"utc_ns":null,"sources":[]}),
        ),
        storage: Some(
            json!({"present":true,"capture_root":"/mnt/nvme0/ados/capture",
            "volumes":[{"mount":"/mnt/nvme0","device":"nvme0n1","fs":"ext4","total_bytes":1000,"free_bytes":500,"rotational":false,"is_capture":true}],
            "capture":{"used_bytes":340,"budget_bytes":1000,"budget_pct":85,"floor_free_bytes":5,"sessions":12,"uploaded":9,"evict_unuploaded":false,"state":"ok"},
            "write_probe":null}),
        ),
        capture: Some(
            json!({"state":"recording","session_id":"visor-1-20261019T101500Z-a1c3","started_at_ns":120000000000_u64,"duration_s":401.2}),
        ),
        ..ProfileRoutes::default()
    }
}

pub(crate) fn lora_fixture() -> Value {
    json!({"present":true,"transport":"e22","module_band":"850-930",
     "region":{"code":"IN","name":"India","plan_id":"IN865","source":"network.regulatory.region","band_mhz":[865.0,867.0],"citation":"x"},
     "channel":{"freq_mhz":866.0,"bw_khz":125,"sf":9,"erp_mw":25,"max_erp_mw":25},
     "node_id":17,"tx_allowed":true,"blocked_reason":null,
     "governor":{"rules":[
       {"kind":"duty_cycle","window_s":3600,"limit_pct":1.0,"used_pct":0.4,"airtime_ms_window":14400,"budget_ms_window":36000,"exhausted":false,"sub_band":"865.0-867.0"},
       {"kind":"dwell_time","limit_ms":400,"per_s":20,"last_tx_ms":206,"ok":true},
       {"kind":"max_power","limit_mw":25,"configured_mw":25,"ok":true}],
      "next_tx_allowed_ms":0,"last_refusal":null},
     "tx":{"frames":120,"refused":0,"refused_by_rule":{}},
     "rx":{"frames":340,"crc_errors":2}})
}

pub(crate) fn gs_routes() -> ProfileRoutes {
    ProfileRoutes {
        radio: Some(
            json!({"present":true,"adapters":[{"iface":"wlan1","mac":"aa:bb:cc:dd:ee:01","usb_path":"1-1.2","driver":"rtl88x2eu","role":"rxtx","antenna":"omni-5.8","tx_active":true,
            "rssi_dbm":[-48,-51],"packets":999,"lost":1,"fec_rec":540,"verified":true}],
            "diversity":{"rx_adapters":1,"combined_loss_pct":0.1,"best_single_loss_pct":0.1}}),
        ),
        gnss: Some(
            json!({"present":true,"receivers":[{"id":"gnss0","source":"hub","model":"u-blox M10","fix":"3d","sats":14,"hdop":0.8,"rtk":"none","pps_locked":true}],
            "base":{"mode":"survey","state":"surveying","elapsed_s":120,"min_dur_s":300,"accuracy_m":3.1,"target_accuracy_m":2.0,"fixed":null,"rtcm_out":{"msgs_per_s":0.0,"bytes_total":0,"targets":0}}}),
        ),
        lora: Some(lora_fixture()),
        ..ProfileRoutes::default()
    }
}

fn dash(profile: &str, routes: &ProfileRoutes) -> Dashboard {
    let mut d = Dashboard::from_status(
        &json!({"profile": profile, "paired": true, "mavlink": {"connected": false}}),
    );
    d.merge_services(&json!({"services": [{"state": "active"}]}));
    d.merge_profile(routes);
    d
}

fn texts(d: &Dashboard) -> Vec<String> {
    d.profile_row.iter().map(RowLine::text).collect()
}

#[test]
fn route_sets_follow_the_profile() {
    let v = RouteSet::for_profile("visor");
    assert!(v.sensors && v.clock && v.storage && v.capture && !v.lora);
    let g = RouteSet::for_profile("ground_station");
    assert!(g.gnss && g.lora && g.radio && !g.sensors);
    assert_eq!(
        RouteSet::for_profile("drone"),
        RouteSet {
            sensors: true,
            ..RouteSet::default()
        }
    );
    assert_eq!(RouteSet::for_profile("workstation"), RouteSet::default());
    assert_eq!(RouteSet::for_profile("compute"), RouteSet::default());
}

#[test]
fn absent_routes_are_dropped() {
    assert!(present(json!({"present": false, "adapters": []})).is_none());
    assert!(present(json!({"adapters": []})).is_some());
}

#[test]
fn visor_row_matches_the_spec() {
    let d = dash("visor", &visor_routes(true, false));
    assert_eq!(
        texts(&d),
        ["capture REC 06:41 │ visor-1-…-a1c3 │ storage 34 % │ clock σ 0.04 ms │ hub imu 1000 Hz"]
    );
}

#[test]
fn visor_fc_ok_with_hub_only() {
    let d = dash("visor", &visor_routes(true, false));
    assert_eq!(d.health(), Health::Healthy);
    let d = dash("visor", &visor_routes(false, true));
    assert_eq!(d.health(), Health::Healthy);
    let d = dash("visor", &visor_routes(false, false));
    assert_eq!(d.health(), Health::Degraded);
    // Without the sensors route the existing FC signal stands.
    let d = dash("visor", &ProfileRoutes::default());
    assert_eq!(d.health(), Health::Degraded);
}

#[test]
fn visor_without_routes_has_no_row() {
    assert!(dash("visor", &ProfileRoutes::default())
        .profile_row
        .is_empty());
}

#[test]
fn drone_row_is_one_sensor_rate_line() {
    let routes = ProfileRoutes {
        sensors: Some(sensors_fixture(false, true)),
        ..ProfileRoutes::default()
    };
    let d = dash("drone", &routes);
    assert_eq!(
        texts(&d),
        ["sensors imu 1000/1000 Hz · camera 12/20 Hz · gnss 10 Hz"]
    );
    let camera = &d.profile_row[0]
        .0
        .iter()
        .find(|(t, _)| t == "12/20 Hz")
        .unwrap()
        .1;
    assert_eq!(*camera, Level::Warn);
}

#[test]
fn workstation_and_compute_have_no_row() {
    for p in ["workstation", "compute"] {
        assert!(dash(p, &visor_routes(true, true)).profile_row.is_empty());
    }
}

#[test]
fn gs_row_has_radios_gnss_and_lora() {
    let d = dash("ground_station", &gs_routes());
    assert_eq!(
        texts(&d),
        [
            "wlan1   rxtx   -48 dBm  loss 0.1 %",
            "GNSS 3d 14 sats base survey",
            "LoRa IN865 duty 0.4%/1% pwr 25/25mW TX ok  refused 0",
        ]
    );
}

fn lora_text(patch: impl FnOnce(&mut Value)) -> RowLine {
    let mut l = lora_fixture();
    patch(&mut l);
    super::profile_gs::lora_line(&l)
}

#[test]
fn lora_region_unset_line() {
    let line = lora_text(|l| {
        l["region"] = Value::Null;
        l["tx_allowed"] = json!(false);
        l["blocked_reason"] = json!("region_unset");
    });
    assert_eq!(line.text(), "LoRa --    TX BLOCKED region_unset (RX on)");
    assert!(line
        .0
        .iter()
        .any(|(t, lv)| t.starts_with("TX BLOCKED") && *lv == Level::Error));
}

#[test]
fn lora_module_band_mismatch_line() {
    let line = lora_text(|l| {
        l["region"]["plan_id"] = json!("CN470");
        l["tx_allowed"] = json!(false);
        l["blocked_reason"] = json!("module_band_mismatch");
    });
    assert_eq!(line.text(), "LoRa CN470 TX BLOCKED module_band_mismatch");
}

#[test]
fn lora_duty_wait_line() {
    let line = lora_text(|l| {
        l["tx_allowed"] = json!(false);
        l["blocked_reason"] = json!("duty_exhausted");
        l["governor"]["rules"][0]["used_pct"] = json!(1.0);
        l["governor"]["rules"][0]["exhausted"] = json!(true);
        l["governor"]["next_tx_allowed_ms"] = json!(462000);
        l["tx"]["refused"] = json!(2);
        l["governor"]["last_refusal"] = json!({"rule": "duty_cycle", "at_ms": 1});
    });
    assert_eq!(
        line.text(),
        "LoRa IN865 duty 1%/1% pwr 25/25mW TX WAIT duty_exhausted 07:42  refused 2 (duty)"
    );
    assert!(line
        .0
        .iter()
        .any(|(t, lv)| t == "1%/1% " && *lv == Level::Error));
}

#[test]
fn lora_multi_sub_band_shows_two_highest() {
    let line = lora_text(|l| {
        l["region"]["plan_id"] = json!("EU868");
        l["governor"]["rules"] = json!([
            {"kind":"duty_cycle","limit_pct":1.0,"used_pct":0.81,"exhausted":false,"sub_band":"M"},
            {"kind":"duty_cycle","limit_pct":0.1,"used_pct":0.01,"exhausted":false,"sub_band":"L"},
            {"kind":"duty_cycle","limit_pct":10.0,"used_pct":3.0,"exhausted":false,"sub_band":"P"}]);
    });
    assert_eq!(
        line.text(),
        "LoRa EU868 M 0.8%/1% P 3%/10% TX ok  refused 0"
    );
    assert!(line
        .0
        .iter()
        .any(|(t, lv)| t == "0.8%/1% " && *lv == Level::Warn));
}
