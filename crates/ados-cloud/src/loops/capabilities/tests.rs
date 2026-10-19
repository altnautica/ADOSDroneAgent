use super::*;

fn write(dir: &Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).unwrap();
}

const NOW: f64 = 1_700_000_000.0;

#[test]
fn a_node_with_no_sidecars_emits_nothing_rather_than_defaults() {
    // The whole point of the block: absent must read as unknown on the
    // GCS, never as a measured "no camera / link down / powersave off".
    let dir = tempfile::tempdir().unwrap();
    assert!(capability_extras_in(dir.path(), NOW).is_empty());
}

#[test]
fn the_camera_and_reconciler_blocks_cross_the_cloud_wire() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "camera-state.json",
        r#"{"updated_at_unix":1700000000,"state":"ready"}"#,
    );
    write(
        dir.path(),
        "mgmt-link.json",
        r#"{"updated_at_unix":1700000000,"state":"degraded","iface":"eth0","carrier":true}"#,
    );
    write(
        dir.path(),
        "mgmt-failover.json",
        r#"{"updated_at_unix":1700000000,"mgmt_link_mode":"wifi_heartbeat"}"#,
    );
    write(
        dir.path(),
        "usb-rehome.json",
        r#"{"updated_at_unix":1700000000,"usb_rehome_state":"rehoming","usb_rehome_attempts":3}"#,
    );
    write(
        dir.path(),
        "lcd-state.json",
        r#"{"active_page_id":"radio"}"#,
    );

    let got = capability_extras_in(dir.path(), NOW);
    assert_eq!(got.get("cameraState"), Some(&json!("ready")));
    assert_eq!(got["managementLink"]["state"], json!("degraded"));
    assert_eq!(got["managementLink"]["carrier"], json!(true));
    // A key the sidecar omits reads null, never a fabricated default.
    assert_eq!(got["managementLink"]["backend"], Value::Null);
    assert_eq!(got.get("mgmtLinkMode"), Some(&json!("wifi_heartbeat")));
    assert_eq!(got.get("usbRehomeState"), Some(&json!("rehoming")));
    assert_eq!(got.get("usbRehomeAttempts"), Some(&json!(3)));
    assert_eq!(got.get("lcdActivePage"), Some(&json!("radio")));
}

#[test]
fn a_stale_reading_is_dropped_rather_than_republished() {
    // A stopped reconciler's last verdict must not keep rendering as live.
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "camera-state.json",
        r#"{"updated_at_unix":1699990000,"state":"ready"}"#,
    );
    write(
        dir.path(),
        "mgmt-link.json",
        r#"{"updated_at_unix":1699990000,"state":"healthy"}"#,
    );
    let got = capability_extras_in(dir.path(), NOW);
    assert!(
        !got.contains_key("cameraState"),
        "10 000 s past a 300 s window"
    );
    assert!(!got.contains_key("managementLink"), "past the 90 s window");
}

#[test]
fn an_unrecognised_verdict_is_dropped_rather_than_shipped() {
    // The GCS clamp accepts only a known state; shipping an unknown one
    // renders an empty card with no explanation.
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "camera-state.json",
        r#"{"updated_at_unix":1700000000,"state":"warming-up"}"#,
    );
    assert!(!capability_extras_in(dir.path(), NOW).contains_key("cameraState"));
}

#[test]
fn a_detected_camera_behind_a_failed_pipeline_reads_error_not_ready() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "camera-state.json",
        r#"{"updated_at_unix":1700000000,"state":"ready","pipeline_state":"error"}"#,
    );
    assert_eq!(
        capability_extras_in(dir.path(), NOW).get("cameraState"),
        Some(&json!("error")),
        "a confident camera pill over a dead pane is the defect"
    );
}

#[test]
fn a_node_with_no_managed_wifi_omits_the_block_instead_of_an_empty_list() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "wifi-powersave.json", r#"{"interfaces":{}}"#);
    assert!(!capability_extras_in(dir.path(), NOW).contains_key("wifiPowersave"));
}

#[test]
fn the_wifi_powersave_object_is_reshaped_into_the_array_the_gcs_clamps() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "wifi-powersave.json",
        r#"{"interfaces":{"wlan0":{"powersave_on":false,"reasserts":2,"signal_dbm":-54}}}"#,
    );
    let got = capability_extras_in(dir.path(), NOW);
    let rows = got["wifiPowersave"]["interfaces"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["iface"], json!("wlan0"));
    assert_eq!(rows[0]["powersaveOn"], json!(false));
    assert_eq!(rows[0]["reasserts"], json!(2));
    assert_eq!(rows[0]["signalDbm"], json!(-54));
    assert_eq!(rows[0]["linkState"], json!("unknown"));
}

#[test]
fn node_flags_match_the_lan_presence_rules() {
    let run = tempfile::tempdir().unwrap();
    let etc = tempfile::tempdir().unwrap();
    let now = std::time::SystemTime::now();
    let flags = node_flags_in("ground-station", run.path(), etc.path(), false, now);
    assert_eq!(
        Value::Object(flags),
        json!({"gnss": false, "lora": false, "sim": false})
    );

    write(run.path(), "gnss.json", "{}");
    write(etc.path(), "lora-enabled", "");
    let flags = node_flags_in("ground-station", run.path(), etc.path(), true, now);
    assert_eq!(
        Value::Object(flags),
        json!({"gnss": true, "lora": true, "sim": true})
    );

    // A visor runs neither lane's unit, whatever the files say.
    let flags = node_flags_in("visor", run.path(), etc.path(), false, now);
    assert_eq!(flags["lora"], json!(false));
}
