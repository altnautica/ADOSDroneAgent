use super::*;

mod sidecars;

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
    // The node lane flags always ride as booleans so a relay-only node gets
    // the same GNSS/LoRa/sim answer `/api/status/full` gives on the LAN.
    for key in ["gnss", "lora", "sim"] {
        assert!(obj[key].is_boolean(), "{key} must be a boolean");
    }
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
