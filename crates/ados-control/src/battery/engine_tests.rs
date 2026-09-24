use serde_json::json;

use super::*;

/// A state snapshot carrying the given `batteries[]` entries, the FC link up
/// and an empty SYS_STATUS battery block.
fn snap(packs: Value) -> Value {
    json!({
        "mavlink_alive": true,
        "battery": {
            "voltage": null,
            "current": null,
            "remaining": null,
            "temperature": null,
            "cell_voltages": [],
        },
        "batteries": packs,
    })
}

/// One `batteries[]` entry as the router publishes it.
fn pack(id: u8, cells: &[f64]) -> Value {
    json!({
        "id": id,
        "cell_voltages": cells,
        "current_a": -10.0,
        "remaining_pct": 80,
        "temperature_c": 30.0,
        "consumed_mah": 1200,
        "consumed_wh": 17.5,
        "function": 1,
    })
}

const HEALTHY: [f64; 4] = [3.9, 3.9, 3.9, 3.9];
/// Under the low threshold with the cells matched, so only `cell_low` fires.
const LOW: [f64; 4] = [3.49, 3.49, 3.49, 3.47];
/// Back over the low threshold, within a sag rate of `LOW`.
const RECOVERED: [f64; 4] = [3.52, 3.52, 3.52, 3.52];

fn engine() -> BatteryEngine {
    BatteryEngine::new(BatteryConfig::default())
}

fn wire(engine: &BatteryEngine, now_ms: i64) -> Value {
    serde_json::to_value(engine.snapshot(now_ms)).unwrap()
}

#[test]
fn the_first_sample_creates_the_pack_and_sets_its_reading() {
    let mut e = engine();
    e.ingest(1000, &snap(json!([pack(0, &HEALTHY)])));
    let w = wire(&e, 1000);
    assert_eq!(w["packs"].as_array().unwrap().len(), 1);
    let p = &w["packs"][0];
    assert_eq!(p["voltage_v"], json!(15.6));
    assert_eq!(p["cells_plausible"], json!(true));
    assert_eq!(p["min_cell_v"], json!(3.9));
    assert_eq!(p["divergence_mv"], json!(0.0));
    assert_eq!(p["consumed_mah"], json!(1200));
    assert_eq!(p["prediction"]["state"], json!("idle"));
    assert_eq!(w["stale"], json!(false));
    assert_eq!(w["updated_at_ms"], json!(1000));
}

#[test]
fn the_sample_window_keeps_the_newest_readings_at_its_cap() {
    let mut e = engine();
    for i in 0..(SAMPLE_CAP as i64 + 12) {
        e.ingest(1000 + i * 500, &snap(json!([pack(0, &HEALTHY)])));
    }
    let points = &e.packs[&0].points;
    assert_eq!(points.len(), SAMPLE_CAP);
    assert_eq!(points.front().unwrap().t_ms, 1000 + 12 * 500);
}

#[test]
fn a_rule_that_keeps_firing_raises_once() {
    let mut e = engine();
    let first = e.ingest(1000, &snap(json!([pack(0, &LOW)])));
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].rule, RuleId::CellLow);
    assert_eq!(first[0].state, TransitionState::Raised);
    assert_eq!(first[0].pack_id, 0);
    let again = e.ingest(2000, &snap(json!([pack(0, &LOW)])));
    assert!(again.is_empty());
    let live = &wire(&e, 2000)["packs"][0]["anomalies"];
    assert_eq!(live.as_array().unwrap().len(), 1);
    assert_eq!(live[0]["first_seen_ms"], json!(1000));
    assert_eq!(live[0]["last_seen_ms"], json!(2000));
}

#[test]
fn an_anomaly_clears_only_after_the_hysteresis_window() {
    let mut e = engine();
    e.ingest(1000, &snap(json!([pack(0, &LOW)])));
    // The condition lifts: the anomaly stays live, stamped with when.
    assert!(e
        .ingest(2000, &snap(json!([pack(0, &RECOVERED)])))
        .is_empty());
    let live = wire(&e, 2000)["packs"][0]["anomalies"][0].clone();
    assert_eq!(live["cleared_at_ms"], json!(2000));
    // Still inside 5 s of the stamp.
    assert!(e
        .ingest(5000, &snap(json!([pack(0, &RECOVERED)])))
        .is_empty());
    assert!(e
        .ingest(6999, &snap(json!([pack(0, &RECOVERED)])))
        .is_empty());
    // 5 s after the stamp: the final clear.
    let cleared = e.ingest(7000, &snap(json!([pack(0, &RECOVERED)])));
    assert_eq!(cleared.len(), 1);
    assert_eq!(cleared[0].rule, RuleId::CellLow);
    assert_eq!(cleared[0].state, TransitionState::Cleared);
    let w = wire(&e, 7000);
    assert!(w["packs"][0]["anomalies"].as_array().unwrap().is_empty());
    let history = &w["packs"][0]["history"];
    assert_eq!(history[0]["state"], json!("cleared"));
    assert_eq!(history[0]["at_ms"], json!(7000));
    assert_eq!(history[1]["state"], json!("raised"));
    assert_eq!(history[1]["at_ms"], json!(1000));
}

#[test]
fn a_re_fire_inside_the_window_keeps_the_anomaly_without_a_new_raise() {
    let mut e = engine();
    e.ingest(1000, &snap(json!([pack(0, &LOW)])));
    e.ingest(2000, &snap(json!([pack(0, &RECOVERED)])));
    assert!(e.ingest(4000, &snap(json!([pack(0, &LOW)]))).is_empty());
    let live = wire(&e, 4000)["packs"][0]["anomalies"][0].clone();
    assert_eq!(live["cleared_at_ms"], Value::Null);
    assert_eq!(live["first_seen_ms"], json!(1000));
    // The window restarts from the next lift, not from the first one.
    e.ingest(5000, &snap(json!([pack(0, &RECOVERED)])));
    assert!(e
        .ingest(8000, &snap(json!([pack(0, &RECOVERED)])))
        .is_empty());
    assert_eq!(
        e.ingest(10_000, &snap(json!([pack(0, &RECOVERED)]))).len(),
        1
    );
}

#[test]
fn a_config_change_reprojects_without_waiting_for_a_sample() {
    let mut e = engine();
    // 1 %/s from 95 %.
    for s in 0..10 {
        let mut p = pack(0, &HEALTHY);
        p["remaining_pct"] = json!(95 - s);
        e.ingest(s * 1000, &snap(json!([p])));
    }
    let before = wire(&e, 9000)["packs"][0]["prediction"]["eta_s"].clone();
    assert_eq!(before, json!(61));
    e.set_config(BatteryConfig {
        reserve_percent: 50,
        ..BatteryConfig::default()
    });
    let after = wire(&e, 9000)["packs"][0]["prediction"].clone();
    assert_eq!(after["eta_s"], json!(36));
    assert_eq!(after["state"], json!("high"));
}

#[test]
fn each_pack_is_tracked_on_its_own() {
    let mut e = engine();
    let raised = e.ingest(1000, &snap(json!([pack(0, &HEALTHY), pack(2, &LOW)])));
    assert_eq!(raised.len(), 1);
    assert_eq!(raised[0].pack_id, 2);
    let w = wire(&e, 1000);
    let ids: Vec<&Value> = w["packs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| &p["id"])
        .collect();
    assert_eq!(ids, [&json!(0), &json!(2)]);
    assert!(w["packs"][0]["anomalies"].as_array().unwrap().is_empty());
    assert_eq!(w["packs"][1]["weakest_cell_index"], json!(3));
}

/// ArduPilot with no per-cell monitor: the whole pack in `voltages[0]`. It is
/// reported as the pack voltage, never as a cell.
#[test]
fn a_whole_pack_value_in_the_cell_array_is_the_pack_voltage() {
    let mut e = engine();
    let raised = e.ingest(1000, &snap(json!([pack(0, &[16.8])])));
    assert!(raised.is_empty());
    let p = &wire(&e, 1000)["packs"][0];
    assert_eq!(p["cells_plausible"], json!(false));
    assert_eq!(p["cell_voltages_v"], json!([16.8]));
    assert_eq!(p["voltage_v"], json!(16.8));
    for key in [
        "weakest_cell_index",
        "min_cell_v",
        "max_cell_v",
        "divergence_mv",
    ] {
        assert_eq!(p[key], Value::Null, "{key}");
    }
}

#[test]
fn pack_zero_falls_back_to_sys_status_and_other_packs_do_not() {
    let mut s = snap(json!([
        {"id": 0, "cell_voltages": [], "current_a": null, "remaining_pct": null},
        {"id": 1, "cell_voltages": [], "current_a": null, "remaining_pct": null},
    ]));
    s["battery"]["voltage"] = json!(12.4);
    s["battery"]["remaining"] = json!(64);
    let mut e = engine();
    e.ingest(1000, &s);
    let w = wire(&e, 1000);
    assert_eq!(w["packs"][0]["voltage_v"], json!(12.4));
    assert_eq!(w["packs"][0]["remaining_pct"], json!(64));
    assert_eq!(w["packs"][1]["voltage_v"], Value::Null);
    assert_eq!(w["packs"][1]["remaining_pct"], Value::Null);
}

#[test]
fn a_sys_status_only_fc_reads_as_pack_zero() {
    let mut s = snap(json!([]));
    s["battery"] = json!({
        "voltage": 11.1,
        "current": 4.5,
        "remaining": 70,
        "temperature": null,
        "cell_voltages": [],
    });
    let mut e = engine();
    e.ingest(1000, &s);
    let p = &wire(&e, 1000)["packs"][0];
    assert_eq!(p["id"], json!(0));
    assert_eq!(p["voltage_v"], json!(11.1));
    assert_eq!(p["current_a"], json!(4.5));
    assert_eq!(p["remaining_pct"], json!(70));

    // No battery data at all is no pack, not an all-null one.
    let mut empty = engine();
    empty.ingest(1000, &snap(json!([])));
    assert!(wire(&empty, 1000)["packs"].as_array().unwrap().is_empty());
}

#[test]
fn a_dead_fc_link_is_not_sampled_and_goes_stale() {
    let mut e = engine();
    e.ingest(1000, &snap(json!([pack(0, &HEALTHY)])));
    let mut down = snap(json!([pack(0, &LOW)]));
    down["mavlink_alive"] = json!(false);
    assert!(e.ingest(2000, &down).is_empty());
    let w = wire(&e, 6000);
    assert_eq!(w["updated_at_ms"], json!(1000));
    assert_eq!(w["stale"], json!(false));
    assert_eq!(wire(&e, 6001)["stale"], json!(true));
}

#[test]
fn nothing_ingested_yet_is_stale() {
    let w = wire(&engine(), 1000);
    assert_eq!(w["stale"], json!(true));
    assert_eq!(w["updated_at_ms"], json!(0));
}

#[test]
fn disabled_serves_no_packs_and_raises_nothing() {
    let mut e = engine();
    e.ingest(1000, &snap(json!([pack(0, &LOW)])));
    e.set_config(BatteryConfig {
        enabled: false,
        ..BatteryConfig::default()
    });
    assert!(e.ingest(2000, &snap(json!([pack(0, &LOW)]))).is_empty());
    let w = wire(&e, 2000);
    assert_eq!(w["enabled"], json!(false));
    assert!(w["packs"].as_array().unwrap().is_empty());
    // Re-enabling starts clean: the old anomaly is raised afresh.
    e.set_config(BatteryConfig::default());
    assert_eq!(e.ingest(3000, &snap(json!([pack(0, &LOW)]))).len(), 1);
}

#[test]
fn a_voltage_sag_across_samples_raises_voltage_drop() {
    let mut e = engine();
    e.ingest(1000, &snap(json!([pack(0, &[4.1, 4.1, 4.1])])));
    let raised = e.ingest(1500, &snap(json!([pack(0, &[3.95, 3.95, 3.95])])));
    let drop = raised
        .iter()
        .find(|t| t.rule == RuleId::VoltageDrop)
        .expect("voltage_drop raised");
    // 12.3 V -> 11.85 V in 0.5 s.
    assert!((drop.value - 0.9).abs() < 1e-9, "{}", drop.value);
}

#[test]
fn pack_ids_past_the_cap_are_ignored() {
    let mut e = engine();
    let packs: Vec<Value> = (0..=MAX_PACKS as u8).map(|id| pack(id, &HEALTHY)).collect();
    e.ingest(1000, &snap(Value::Array(packs)));
    assert_eq!(e.packs.len(), MAX_PACKS);
    assert!(!e.packs.contains_key(&(MAX_PACKS as u8)));
}
