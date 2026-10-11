use super::*;

#[tokio::test]
async fn reconcile_wifi_powersave_runs_without_a_logd_socket() {
    // The reconcile must never panic when the logd socket is absent. On a
    // non-Linux dev host it is an inert no-op; on Linux CI it shells `iw`
    // read-only and falls through safely when `iw` is unavailable or there
    // are no station interfaces. Either way: no panic.
    let events = EventEmitter::with_socket("ados-test", "/nonexistent/ados/logd.sock");
    let mut reasserts = std::collections::HashMap::new();
    reconcile_wifi_powersave(&mut reasserts, &events).await;
}

#[test]
fn reassert_detail_is_bland_and_names_the_action() {
    let d = reassert_detail("wlan0");
    assert_eq!(d.get("source").and_then(|v| v.as_str()), Some("supervisor"));
    assert_eq!(d.get("iface").and_then(|v| v.as_str()), Some("wlan0"));
    assert_eq!(d.get("from").and_then(|v| v.as_str()), Some("on"));
    assert_eq!(d.get("to").and_then(|v| v.as_str()), Some("off"));
}

#[test]
fn now_iso8601_is_a_utc_offset_string() {
    let s = now_iso8601();
    // YYYY-MM-DDTHH:MM:SS+00:00 — 25 chars, ending in a +00:00 offset.
    assert!(s.ends_with("+00:00"), "got {s}");
    assert_eq!(s.len(), 25, "got {s}");
}

#[test]
fn sidecar_round_trips_through_the_atomic_writer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wifi-powersave.json");
    let mut interfaces = std::collections::BTreeMap::new();
    interfaces.insert(
        "wlan0".to_string(),
        WifiIfaceSnapshot {
            powersave_on: false,
            reasserts: 3,
            last_reassert: Some("2026-07-04T10:00:00+00:00".to_string()),
            signal_dbm: Some(-52),
            link_state: "connected".to_string(),
        },
    );
    let snap = WifiPowersaveSidecar {
        version: WIFI_POWERSAVE_SIDECAR_VERSION,
        interfaces,
    };
    write_json_atomic(&path, &snap, 0o644).unwrap();
    let reloaded: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(reloaded["version"], WIFI_POWERSAVE_SIDECAR_VERSION);
    assert_eq!(reloaded["interfaces"]["wlan0"]["powersave_on"], false);
    assert_eq!(reloaded["interfaces"]["wlan0"]["reasserts"], 3);
    assert_eq!(
        reloaded["interfaces"]["wlan0"]["last_reassert"],
        "2026-07-04T10:00:00+00:00"
    );
    assert_eq!(reloaded["interfaces"]["wlan0"]["signal_dbm"], -52);
    assert_eq!(reloaded["interfaces"]["wlan0"]["link_state"], "connected");
    // No leftover temp sibling.
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn sidecar_version_matches_registry() {
    // The per-file const and the sidecar registry are the two sources of
    // truth for this sidecar's schema version; a drift is caught here.
    assert_eq!(
        WIFI_POWERSAVE_SIDECAR_VERSION,
        ados_protocol::contracts::sidecar_version("wifi-powersave").unwrap()
    );
}
