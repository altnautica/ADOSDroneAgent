use super::*;

/// Serve exactly one HTTP request on loopback, answering `body` as JSON,
/// and hand back the raw request head the client sent.
async fn capture_one_request(
    body: &'static str,
) -> (String, tokio::sync::oneshot::Receiver<String>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        let mut buf = [0u8; 4096];
        while !head.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = sock.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            head.extend_from_slice(&buf[..n]);
        }
        let _ = tx.send(String::from_utf8_lossy(&head).into_owned());
        let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        let _ = sock.write_all(resp.as_bytes()).await;
    });
    (format!("http://{addr}"), rx)
}

#[tokio::test]
async fn the_heartbeat_key_rides_the_header_never_the_url() {
    let (base, head) = capture_one_request("{}").await;
    let client = reqwest::Client::builder()
        .use_preconfigured_tls(crate::tls::client_config())
        .build()
        .unwrap();
    post_heartbeat(
        &client,
        &base,
        "k-secret",
        &serde_json::json!({"deviceId": "d1"}),
    )
    .await;
    let head = head.await.unwrap();
    let request_line = head.lines().next().unwrap();
    assert!(
        request_line.starts_with("POST /agent/status "),
        "{request_line}"
    );
    assert!(
        !request_line.contains("k-secret"),
        "the key leaked into the URL"
    );
    assert!(
        head.to_ascii_lowercase().contains("x-ados-key: k-secret"),
        "{head}"
    );
}

#[test]
fn a_rejected_or_lost_post_never_refreshes_the_last_good_heartbeat() {
    let mut t = CloudLinkTracker::default();
    t.record(&HeartbeatOutcome::Answered(200), 1_000);
    t.record(&HeartbeatOutcome::Answered(401), 6_000);
    let link = t.link(true, true, Some(true), 6_000);
    assert_eq!(link.last_heartbeat_ok_ms, Some(1_000));
    assert_eq!(link.last_heartbeat_status, Some(401));

    t.record(&HeartbeatOutcome::Unanswered("timed out".into()), 11_000);
    let link = t.link(true, true, Some(false), 11_000);
    assert_eq!(link.last_heartbeat_ok_ms, Some(1_000));
    assert_eq!(link.last_heartbeat_status, None);
    assert_eq!(link.last_heartbeat_error.as_deref(), Some("timed out"));
}

fn base() -> HeartbeatBase {
    HeartbeatBase {
        device_id: "dev1".to_string(),
        version: "0.1.0".to_string(),
        profile: Some("drone".to_string()),
        role: None,
        uptime_seconds: 42,
        board_name: "rock-5c-lite".to_string(),
        board_tier: 3,
        board_soc: "rk3582".to_string(),
        board_arch: "aarch64".to_string(),
        board_npu_tops: 6.0,
        board_local_inference: false,
    }
}

#[test]
fn folded_video_legs_advertise_a_same_origin_whep_path_only() {
    // VID-09: the GCS must never be handed an absolute `host:port` media
    // endpoint. The agent proxies `/whep?camera=<id>` on its own :8080
    // (behind the media-auth gate); `:8889/<id>/whep` is a name it does not
    // serve, cannot resolve off-LAN, and is mixed content under an HTTPS
    // ground station.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("video-streams.json");
    let now_ms = 1_800_000_000_000_i64;
    std::fs::write(
        &path,
        serde_json::json!({
            "updated_at_unix": (now_ms as f64) / 1000.0,
            "streams": [
                {"id": "main", "role": "eo", "codec": "h264", "live": true},
                {"id": "ir", "role": "ir", "codec": "h264"},
            ],
        })
        .to_string(),
    )
    .unwrap();

    let legs = read_video_streams_sidecar_from(&path, now_ms).expect("fresh sidecar folds");
    assert_eq!(
        legs.iter()
            .map(|l| l.whep_url.clone().unwrap())
            .collect::<Vec<_>>(),
        vec!["/whep?camera=main", "/whep?camera=ir"],
    );
    // On the wire: the relative path under `whepUrl`, no port anywhere, and
    // no `hlsUrl` (nothing consuming this payload can play HLS).
    let wire = serde_json::to_value(&legs).unwrap();
    let text = wire.to_string();
    assert!(text.contains("\"whepUrl\":\"/whep?camera=main\""), "{text}");
    assert!(!text.contains("8889"), "no advertised port: {text}");
    assert!(!text.contains("hls"), "no unplayable reach: {text}");
    // Liveness still rides through untouched.
    assert_eq!(legs[0].live, Some(true));
    assert_eq!(legs[1].live, None);
}

fn write_named(dir: &std::path::Path, name: &str, body: &str) {
    use std::io::Write;
    std::fs::create_dir_all(dir).unwrap();
    std::fs::File::create(dir.join(name))
        .unwrap()
        .write_all(body.as_bytes())
        .unwrap();
}

#[test]
fn fresh_plugin_sidecars_fold_keyed_by_id_with_the_slice_verbatim() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    write_named(
        &dir,
        "com.example.world-state.json",
        r#"{"state":"active","gaussianCount":42}"#,
    );
    write_named(&dir, "follow-me-state.json", r#"{"lock":"locked"}"#);
    write_named(&dir, "bad-state.json", "{ not json");
    write_named(&dir, "notes.txt", "ignored: not a *-state.json file");

    let out = read_plugin_state_sidecars_from(&dir, std::time::SystemTime::now());
    // Keyed by id (filename minus -state.json); the slice is opaque/verbatim.
    assert_eq!(out["com.example.world"]["state"], "active");
    assert_eq!(out["com.example.world"]["gaussianCount"], 42);
    assert_eq!(out["follow-me"]["lock"], "locked");
    // Malformed JSON + non-state files are skipped, never the whole read.
    assert!(!out.contains_key("bad"));
    assert!(!out.contains_key("notes"));
}

#[test]
fn a_stale_plugin_sidecar_is_dropped() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    write_named(
        &dir,
        "com.example.world-state.json",
        r#"{"state":"active"}"#,
    );
    // A reference `now` an hour after the just-written file -> past the gate.
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
    assert!(read_plugin_state_sidecars_from(&dir, later).is_empty());
}

#[test]
fn an_absent_plugin_dir_is_an_empty_map() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("does-not-exist");
    assert!(read_plugin_state_sidecars_from(&dir, std::time::SystemTime::now()).is_empty());
}

#[test]
fn linked_peers_fold_fresh_remap_camelcase_and_drop_stale_and_idless() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let now_s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let now_ms = (now_s * 1000.0) as i64;
    // A fresh peer, a stale one (2 min old, past the 60 s window), and an
    // id-less entry — only the fresh, id-bearing peer survives.
    let body = format!(
        r#"{{"version":1,"wall_time_unix":{now_s},"peers":[
                {{"device_id":"drone-a","role":"drone","channel":149,"rssi_dbm":-51,"last_seen_unix":{now_s}}},
                {{"device_id":"drone-old","role":"drone","channel":157,"rssi_dbm":-70,"last_seen_unix":{}}},
                {{"device_id":"","role":"drone","channel":153,"rssi_dbm":-60,"last_seen_unix":{now_s}}}
            ]}}"#,
        now_s - 120.0
    );
    write_named(&dir, "linked-peers.json", &body);
    let out = read_linked_peers_sidecar_from(&dir.join("linked-peers.json"), now_ms).unwrap();
    assert_eq!(out.len(), 1);
    // The wire keys are the camelCase set the GCS `LinkedPeer` consumes and
    // the strict cloud validator declares (nothing snake_case survives).
    let v = serde_json::to_value(&out[0]).unwrap();
    assert_eq!(v["deviceId"], "drone-a");
    assert_eq!(v["role"], "drone");
    assert_eq!(v["channel"], 149);
    assert_eq!(v["rssiDbm"], -51);
    assert!((v["seenAtUnix"].as_f64().unwrap() - now_s).abs() < 1.0);
    for k in v.as_object().unwrap().keys() {
        assert!(!k.contains('_'), "{k} must be camelCase on the wire");
    }
}

#[test]
fn linked_peers_absent_when_missing_or_every_entry_stale() {
    // A missing file reads None so a drone / a peerless ground node stays
    // byte-identical (never an empty `linkedPeers`).
    let missing = std::path::Path::new("/run/ados/does-not-exist-linked-peers.json");
    assert!(read_linked_peers_sidecar_from(missing, 1_000_000).is_none());
    // A file whose only peer is stale reads None — the dead-writer case
    // never republishes a ghost peer as a confident list.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    write_named(
        &dir,
        "linked-peers.json",
        r#"{"peers":[{"device_id":"d","role":"drone","channel":1,"rssi_dbm":-1,"last_seen_unix":1.0}]}"#,
    );
    // A reference `now` far in the future puts the sole entry past the gate.
    assert!(
        read_linked_peers_sidecar_from(&dir.join("linked-peers.json"), 10_000_000_000).is_none()
    );
}

#[test]
fn a_fresh_crsf_sidecar_folds_and_a_stale_or_bad_one_reads_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    // The exact sidecar body the lane writes, including its extra derived
    // `flyable` field — outside the pinned block, ignored by the parse.
    write_named(
        &dir,
        "crsf-stats.json",
        r#"{"v":1,"state":"link_ok","rssi_dbm":-51,"lq_uplink":99,"lq_downlink":97,
                "snr_db":8,"band":null,"packet_rate_hz":150,"tx_power_mw":100,
                "tx_frames_per_s":149.8,"rx_frames_per_s":12.0,"rf_unverified":false,
                "flyable":true,"mode":"crsf_rc","channel_source":"hid","relay_role":null,
                "fc_command_down_gated":null}"#,
    );
    let path = dir.join("crsf-stats.json");
    let fresh = read_crsf_sidecar_from(&path, std::time::SystemTime::now()).unwrap();
    assert_eq!(fresh.state.as_deref(), Some("link_ok"));
    assert_eq!(fresh.rssi_dbm, Some(-51));
    assert_eq!(fresh.packet_rate_hz, Some(150));
    // The measured TX power carries under the sidecar's real key. A
    // `tx_power_dbm` field here silently dropped it to null (serde ignored
    // the unmatched key), so a cloud-reached node showed no power.
    assert_eq!(fresh.tx_power_mw, Some(100));
    assert_eq!(fresh.rf_unverified, Some(false));
    // A crsf_rc lane has no MAVLink-over-ELRS command path to gate.
    assert!(fresh.fc_command_down_gated.is_none());
    assert_eq!(fresh.band, None);
    assert_eq!(fresh.relay_role, None);
    // Stale mtime (a reference `now` an hour later) → absent, so a dead
    // lane service's lingering file stops asserting a live lane.
    let later = std::time::SystemTime::now() + Duration::from_secs(3600);
    assert!(read_crsf_sidecar_from(&path, later).is_none());
    // Missing + malformed both read absent, never a default block.
    assert!(read_crsf_sidecar_from(&dir.join("nope.json"), std::time::SystemTime::now()).is_none());
    write_named(&dir, "crsf-bad.json", "{ not json");
    assert!(
        read_crsf_sidecar_from(&dir.join("crsf-bad.json"), std::time::SystemTime::now()).is_none()
    );
}

#[test]
fn a_mavlink_standby_crsf_sidecar_carries_the_command_down_gate() {
    // The exact body the lane writes while standing by in mavlink mode: the
    // MAVLink router owns the carrier telemetry-only, so the command-down
    // gate reads true. A consumer of only the crsf block must see it.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    write_named(
        &dir,
        "crsf-stats.json",
        r#"{"v":1,"state":"ready","rssi_dbm":null,"lq_uplink":null,"lq_downlink":null,
                "snr_db":null,"band":null,"packet_rate_hz":null,"tx_power_mw":null,
                "tx_frames_per_s":null,"rx_frames_per_s":null,"rf_unverified":null,
                "flyable":false,"mode":"mavlink","channel_source":null,"pic":null,
                "relay_role":null,"fc_command_down_gated":true}"#,
    );
    let path = dir.join("crsf-stats.json");
    let fresh = read_crsf_sidecar_from(&path, std::time::SystemTime::now()).unwrap();
    assert_eq!(fresh.state.as_deref(), Some("ready"));
    assert_eq!(fresh.mode.as_deref(), Some("mavlink"));
    assert_eq!(fresh.fc_command_down_gated, Some(true));
}

#[test]
fn config_status_sidecars_surface_only_live_errors_sorted_by_service() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    // Two faulty services (unsorted on disk), one healthy (null error → skip).
    write_named(
        &dir,
        "config-status-mavlink.json",
        r#"{"service":"mavlink","error":"invalid type: string, expected u32","generated_at_ms":1}"#,
    );
    write_named(
        &dir,
        "config-status-cloud.json",
        r#"{"service":"cloud","error":"unknown field `bogus`","generated_at_ms":2}"#,
    );
    write_named(
        &dir,
        "config-status-supervisor.json",
        r#"{"service":"supervisor","error":null,"generated_at_ms":3}"#,
    );
    // Non-matching + staging + malformed files are ignored, never the read.
    write_named(&dir, "config-status-bad.json", "{ not json");
    write_named(
        &dir,
        "config-status-ground_station.json.tmp.999",
        r#"{"service":"ground_station","error":"x"}"#,
    );
    write_named(&dir, "notes.txt", "unrelated");

    let out = read_config_error_sidecars_from(&dir);
    // Only the two live errors, sorted by service (cloud before mavlink).
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].service, "cloud");
    assert_eq!(out[0].error, "unknown field `bogus`");
    assert_eq!(out[1].service, "mavlink");
    // The healthy (null-error) service and the staging/malformed/other files
    // are absent.
    assert!(!out.iter().any(|e| e.service == "supervisor"));
    assert!(!out.iter().any(|e| e.service == "ground_station"));
    assert!(!out.iter().any(|e| e.service == "bad"));
}

#[test]
fn an_absent_config_status_dir_is_an_empty_list() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("does-not-exist");
    assert!(read_config_error_sidecars_from(&dir).is_empty());
}

#[test]
fn config_status_version_matches_registry() {
    // The const lives in the writer crate (ados-config); this reader crate
    // sees both it and the shared registry, so the drift gate lives here.
    assert_eq!(
        ados_config::CONFIG_STATUS_SIDECAR_VERSION,
        ados_protocol::contracts::sidecar_version("config-status").unwrap()
    );
}

#[test]
fn native_only_payload_has_required_fields_and_strips_optionals() {
    let v = build_payload(&base(), None);
    let obj = v.as_object().unwrap();
    // Required-on-wire present.
    assert_eq!(obj["deviceId"], "dev1");
    assert_eq!(obj["version"], "0.1.0");
    assert_eq!(obj["uptimeSeconds"], 42);
    assert_eq!(obj["boardName"], "rock-5c-lite");
    // role None is stripped; no optional enrichment present.
    assert!(!obj.contains_key("role"));
    assert!(!obj.contains_key("temperature"));
    assert!(!obj.contains_key("peripherals"));
    // The unmeasured-by-native fields are OMITTED, not asserted as 0/false/
    // "stopped"/[]. They reappear only via the producer.
    assert!(!obj.contains_key("cpuPercent"));
    assert!(!obj.contains_key("memoryPercent"));
    assert!(!obj.contains_key("diskPercent"));
    assert!(!obj.contains_key("fcConnected"));
    assert!(!obj.contains_key("services"));
    assert!(!obj.contains_key("videoState"));
    // No top-level key is JSON null OUTSIDE the declared null-tolerant set.
    for (k, val) in obj {
        assert!(
            !val.is_null() || UNMEASURED_NULL_KEYS.contains(&k.as_str()),
            "{k} must not be null on the wire"
        );
    }
    // radio is the absent block.
    assert_eq!(obj["radio"]["state"], "absent");
}

#[test]
fn an_unmeasured_null_tolerant_field_rides_as_an_explicit_null_never_omitted() {
    // The cross-repo half of the stale-cloud-status defect. The receiver
    // builds its patch from its own declared column list and Convex drops
    // undefined-valued keys during argument serialization, so a key this
    // agent OMITS carries no instruction at all: the previous tick's value
    // stays in the row and the GCS renders minutes-old radio/link telemetry
    // as live. A declared-but-unmeasured field must therefore ride as an
    // explicit null, which is the form the receiver's
    // `v.optional(v.union(T, v.null()))` column accepts and clears on.
    let v = build_payload(&base(), None);
    let obj = v.as_object().unwrap();
    for key in UNMEASURED_NULL_KEYS {
        // Only the keys this native base owns are asserted present here;
        // the capability-sidecar ones appear when their producer runs.
        if !obj.contains_key(*key) {
            continue;
        }
        assert!(
            obj[*key].is_null(),
            "{key} is unmeasured, so it must be an explicit null"
        );
    }
    // The two the native base always owns: no radio view ⇒ no scan ⇒ the
    // verdict is present-and-null, not missing.
    assert!(
        obj.contains_key("wfbAdapterInjectionOk"),
        "an unmeasured adapter verdict must be SENT as null, not omitted"
    );
    assert!(obj["wfbAdapterInjectionOk"].is_null());
    assert!(obj.contains_key("wfbAdapterChipset"));
    assert!(obj["wfbAdapterChipset"].is_null());

    // A MEASURED verdict still rides as its value, so null stays distinct
    // from a real scanned-and-found-none outcome.
    let enrich = serde_json::json!({ "wfbAdapterInjectionOk": false });
    let v = build_payload(&base(), Some(&enrich));
    assert_eq!(v["wfbAdapterInjectionOk"], serde_json::json!(false));
}

#[test]
fn a_producers_explicit_null_survives_the_strip() {
    // `capability_extras` deliberately emits null for a reach-back /
    // USB-rehome field it has no value for (its `or_null`). The blanket
    // strip used to delete those nulls off the wire, so a guardian that went
    // from "failing over on wlan0" to "nothing to report" left `wlan0`
    // standing in the cloud row forever.
    let enrich = serde_json::json!({
        "mgmtLinkMode": "primary",
        "mgmtFailoverIface": serde_json::Value::Null,
        "mgmtFailoverReason": serde_json::Value::Null,
        "usbRehomeState": "idle",
        "usbRehomeLastResult": serde_json::Value::Null,
        // A key OUTSIDE the declared set: still stripped, because its
        // receiver column rejects an explicit null outright.
        "temperature": serde_json::Value::Null,
    });
    let v = build_payload(&base(), Some(&enrich));
    let obj = v.as_object().unwrap();
    assert!(obj.contains_key("mgmtFailoverIface"));
    assert!(obj["mgmtFailoverIface"].is_null());
    assert!(obj.contains_key("mgmtFailoverReason"));
    assert!(obj["mgmtFailoverReason"].is_null());
    assert!(obj.contains_key("usbRehomeLastResult"));
    assert!(obj["usbRehomeLastResult"].is_null());
    assert_eq!(obj["mgmtLinkMode"], "primary");
    assert!(
        !obj.contains_key("temperature"),
        "a v.optional(T) column rejects an explicit null, so it stays stripped"
    );
}

#[test]
fn the_advertised_reach_is_omitted_rather_than_blanked() {
    // `""` is not "unknown" on the receiver: `stringField` type-checks an
    // empty string straight into the column, so every tick destroyed a
    // previously-good reach and the GCS built `http://:8080` out of it.
    let v = build_payload(&base(), None);
    let obj = v.as_object().unwrap();
    // Nothing here proves an address or that :8080 accepts, so the three
    // IP-derived reaches are absent rather than blank.
    for key in ["lastIp", "setupUrl", "apiUrl"] {
        assert!(
            !obj.contains_key(key),
            "{key} is unproven, so it must be absent — never an empty string"
        );
    }
    // `mdnsHost` is either absent (no hostname that could be another
    // machine's reach) or the resolvable name — never the empty string this
    // used to send unconditionally.
    match obj.get("mdnsHost") {
        None => {}
        Some(name) => {
            let name = name.as_str().expect("mdnsHost is a string when present");
            assert!(!name.is_empty(), "an advertised reach must not be blank");
            assert_eq!(
                Some(name.to_string()),
                ados_protocol::reach::mdns_hostname(),
                "the advertised reach must be the canonical resolvable name"
            );
        }
    }
    // A producer that does prove a reach still overrides the absence.
    let enrich = serde_json::json!({ "lastIp": "192.168.1.50" });
    let v = build_payload(&base(), Some(&enrich));
    assert_eq!(v["lastIp"], "192.168.1.50");
}

#[test]
fn cloud_beacon_carries_the_perception_tier() {
    // The base() board is an NPU part (6 TOPS), so the beacon reports the
    // accelerator + a `local` tier (pick_tier). The offload target is absent
    // (null-stripped) until a workstation is paired.
    let v = build_payload(&base(), None);
    let obj = v.as_object().unwrap();
    assert_eq!(obj["npuTops"], 6.0);
    assert_eq!(obj["hasAccelerator"], true);
    assert_eq!(obj["perceptionTier"], "local");
    assert!(
        !obj.contains_key("perceptionOffloadTarget"),
        "no paired workstation ⇒ the offload target is absent, not null"
    );
}

#[test]
fn enrichment_folds_over_the_native_base() {
    // The producer supplies CPU/temperature + a populated radio block, in the
    // frozen wire shape. They overlay the native zeros.
    let enrich = serde_json::json!({
        "cpuPercent": 12.5,
        "temperature": 47.0,
        "videoState": "running",
        "videoWhepPort": 8889,
        "radio": {
            "state": "connected",
            "channel": 149,
            "freq_mhz": 5745,
            "paired": true,
            "adapter_injection_ok": true
        },
        "wfbAdapterInjectionOk": true
    });
    let v = build_payload(&base(), Some(&enrich));
    let obj = v.as_object().unwrap();
    assert_eq!(obj["cpuPercent"], 12.5);
    assert_eq!(obj["temperature"], 47.0);
    assert_eq!(obj["videoState"], "running");
    // The required base fields survive the fold.
    assert_eq!(obj["deviceId"], "dev1");
    assert_eq!(obj["uptimeSeconds"], 42);
    // The radio sub-block stays snake_case after the fold.
    assert_eq!(obj["radio"]["freq_mhz"], 5745);
    assert_eq!(obj["radio"]["state"], "connected");
    assert_eq!(obj["wfbAdapterInjectionOk"], true);
}
