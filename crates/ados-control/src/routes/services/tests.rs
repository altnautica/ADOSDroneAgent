use super::*;

#[test]
fn parses_a_running_service_row() {
    // A plain `--plain` row: unit load active sub description.
    let row = "ados-mavlink.service loaded active running ADOS MAVLink router";
    let svc = parse_unit_line(row).unwrap();
    assert_eq!(svc["name"], json!("ados-mavlink"));
    assert_eq!(svc["active"], json!(true));
    assert_eq!(svc["state"], json!("active"));
    assert_eq!(svc["sub_state"], json!("running"));
    assert_eq!(svc["load_state"], json!("loaded"));
    assert_eq!(svc["pid"], Value::Null);
}

#[test]
fn parses_an_inactive_service_row() {
    let row = "ados-cloud.service loaded inactive dead ADOS cloud";
    let svc = parse_unit_line(row).unwrap();
    assert_eq!(svc["name"], json!("ados-cloud"));
    assert_eq!(svc["active"], json!(false));
    assert_eq!(svc["state"], json!("inactive"));
    assert_eq!(svc["sub_state"], json!("dead"));
}

#[test]
fn strips_a_leading_failed_unit_glyph() {
    // systemd prepends a status glyph to failed units; the parser drops it.
    for glyph in ['×', '●', '*'] {
        let row = format!("{glyph} ados-video.service loaded failed failed ADOS Video");
        let svc = parse_unit_line(&row).unwrap();
        assert_eq!(svc["name"], json!("ados-video"), "glyph {glyph}");
        assert_eq!(svc["state"], json!("failed"));
        assert_eq!(svc["sub_state"], json!("failed"));
        assert_eq!(svc["load_state"], json!("loaded"));
    }
}

#[test]
fn skips_blank_and_short_and_non_service_rows() {
    assert!(parse_unit_line("").is_none());
    assert!(parse_unit_line("   ").is_none());
    // Fewer than four columns.
    assert!(parse_unit_line("ados-x.service loaded").is_none());
    // A non-.service unit (e.g. a target glob hit) is filtered out.
    assert!(parse_unit_line("ados-thing.timer loaded active waiting A timer").is_none());
    // A glyph with nothing after it yields an empty remainder → skipped.
    assert!(parse_unit_line("×").is_none());
}

#[test]
fn unit_for_service_maps_basenames_and_short_labels() {
    assert_eq!(
        unit_for_service("ados-video"),
        Some("ados-video.service".to_string())
    );
    assert_eq!(
        unit_for_service("ados-mavlink.service"),
        Some("ados-mavlink.service".to_string())
    );
    assert_eq!(
        unit_for_service("video-pipeline"),
        Some("ados-video.service".to_string())
    );
    assert_eq!(
        unit_for_service("pairing-beacon"),
        Some("ados-cloud.service".to_string())
    );
    assert_eq!(unit_for_service("not-a-thing"), None);
    assert_eq!(unit_for_service(""), None);
}

#[test]
fn unit_from_cgroup_extracts_the_ados_unit() {
    let body = "0::/system.slice/ados.slice/ados-video.service\n";
    assert_eq!(
        unit_from_cgroup(body),
        Some("ados-video.service".to_string())
    );
    // A non-ados slice yields None.
    assert_eq!(unit_from_cgroup("0::/system.slice/sshd.service"), None);
    // `ados-` with no body or no `.service` suffix does not match.
    assert_eq!(unit_from_cgroup("ados-.service"), None);
    assert_eq!(unit_from_cgroup("ados-video"), None);
    assert_eq!(unit_from_cgroup(""), None);
}

#[test]
fn pss_kib_from_rollup_reads_the_first_pss_line() {
    let body = "Rss:  12345 kB\nPss:  6789 kB\nShared_Clean:  100 kB\n";
    assert_eq!(pss_kib_from_rollup(body), 6789);
    // No Pss line → 0.
    assert_eq!(pss_kib_from_rollup("Rss:  100 kB\n"), 0);
    // A malformed Pss line → 0.
    assert_eq!(pss_kib_from_rollup("Pss:  notanumber kB\n"), 0);
    assert_eq!(pss_kib_from_rollup(""), 0);
}

#[test]
fn an_unmeasured_unit_reports_null_memory_not_zero() {
    let mut services = vec![
        json!({"name": "ados-video", "active": true, "state": "active"}),
        json!({"name": "ados-cloud", "active": false, "state": "inactive"}),
    ];
    let mut scan = BTreeMap::new();
    scan.insert("ados-video.service".to_string(), 42.5);
    apply_service_memory(&mut services, &scan);
    assert_eq!(services[0]["memory_mb"], json!(42.5));
    assert!(services[1]["memory_mb"].is_null());
}

#[test]
fn round1_matches_python_rounding() {
    assert_eq!(round1(12.34), 12.3);
    assert_eq!(round1(12.35), 12.4);
    assert_eq!(round1(0.0), 0.0);
}

#[test]
fn process_metrics_has_the_three_keys_with_correct_types() {
    let p = process_metrics();
    assert!(p["pid"].is_number());
    assert_eq!(p["cpu_percent"], json!(0.0));
    assert!(p["memory_mb"].is_number());
    // pid is this process; a sane PID is positive.
    assert!(p["pid"].as_u64().unwrap() > 0);
}

/// Golden-fixture parity: with no systemd unit running (a dev host, the only
/// place these unit tests run), the route serves the systemd-fallback shape.
/// The exact Python JSON for that case is:
///
/// ```json
/// {
///   "services": [ ... one object per ados-*.service unit ... ],
///   "systemd_available": true,   // false when systemctl is absent
///   "process": {"pid": <int>, "cpu_percent": 0.0, "memory_mb": <float>}
/// }
/// ```
///
/// Each `services[i]` is exactly:
/// `{name, active, state, sub_state, pid: null, load_state, memory_mb}`.
///
/// The volatile parts (the live unit list, the PID, the RSS) are not
/// byte-comparable across hosts, so this asserts the envelope keys + types +
/// the per-entry object shape against the golden, which is the parity
/// contract. A representative entry (built from the same `parse_unit_line` +
/// `attach_service_memory` code the route runs) is asserted field-by-field.
#[tokio::test]
async fn route_envelope_matches_the_golden_shape() {
    let body = list_services().await.0;
    let obj = body.as_object().expect("top-level object");

    // The three top-level keys, exactly.
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["process", "services", "systemd_available"]);

    // services is an array.
    assert!(obj["services"].is_array(), "services must be an array");

    // systemd_available is a bool.
    assert!(obj["systemd_available"].is_boolean());

    // process is exactly the three-key block with the right types.
    let process = obj["process"].as_object().expect("process object");
    let mut pkeys: Vec<&str> = process.keys().map(String::as_str).collect();
    pkeys.sort_unstable();
    assert_eq!(pkeys, ["cpu_percent", "memory_mb", "pid"]);
    assert_eq!(process["cpu_percent"], json!(0.0));
    assert!(process["pid"].is_number());
    assert!(process["memory_mb"].is_number());

    // Build a representative service entry through the very same code path
    // the route uses, and assert it against the golden entry shape (the live
    // list may be empty on a dev host with no ados units running, so the
    // per-entry shape is pinned here deterministically).
    let mut entry =
        vec![
            parse_unit_line("ados-mavlink.service loaded active running ADOS MAVLink router")
                .expect("a parseable row"),
        ];
    apply_service_memory(&mut entry, &BTreeMap::new());
    let golden = entry[0].as_object().unwrap();
    let mut ekeys: Vec<&str> = golden.keys().map(String::as_str).collect();
    ekeys.sort_unstable();
    assert_eq!(
        ekeys,
        [
            "active",
            "load_state",
            "memory_mb",
            "name",
            "pid",
            "state",
            "sub_state"
        ]
    );
    assert_eq!(golden["name"], json!("ados-mavlink"));
    assert_eq!(golden["active"], json!(true));
    assert_eq!(golden["state"], json!("active"));
    assert_eq!(golden["sub_state"], json!("running"));
    assert_eq!(golden["load_state"], json!("loaded"));
    assert_eq!(golden["pid"], Value::Null);
    // No scan measured it, so the memory is unknown.
    assert!(golden["memory_mb"].is_null());
}
