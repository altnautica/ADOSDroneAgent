use super::*;
use rmpv::Value as MpVal;

fn mk_fields(pairs: &[(&str, &str)]) -> Fields {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), MpVal::from(*v)))
        .collect()
}

/// The hand-written ingest and read-envelope versions must match the
/// contract registry entries so a future drift is a build-time test failure.
#[test]
fn version_matches_registry() {
    assert_eq!(
        WIRE_VERSION as u16,
        crate::contracts::contract_version("logd.ingest").unwrap()
    );
    assert_eq!(
        ENVELOPE_VERSION as u16,
        crate::contracts::contract_version("logd.read").unwrap()
    );
}

#[test]
fn log_frame_round_trips_through_an_ingest_frame() {
    let mut log = LogFrame::new(1_700_000_000_000_000, "ados-logd", Level::Warn, "hello");
    log.target = Some("logd::db".to_string());
    log.fields.insert("attempt".to_string(), MpVal::from(3u64));
    let frame = IngestFrame::Log(log.clone());

    let wire = frame.encode().unwrap();
    // 4-byte big-endian length prefix + body, same framing as the others.
    assert_eq!(&wire[..4], &(wire.len() as u32 - 4).to_be_bytes());

    let back = IngestFrame::decode(&wire[frame::HEADER_SIZE..]).unwrap();
    assert_eq!(back, IngestFrame::Log(log));
}

#[test]
fn telemetry_event_and_hw_frames_round_trip() {
    let tele = IngestFrame::Telemetry(TelemetryFrame::new(1, "cpu.load", 0.42));
    let evt = IngestFrame::Event(EventFrame::new(2, "radio.lock", "ados-radio", Level::Info));
    let mut hw = HwSnapshot::new(3);
    hw.signals
        .insert("thermal.soc_c".to_string(), MpVal::from(54.5));
    let hw = IngestFrame::Hw(hw);

    for frame in [tele, evt, hw] {
        let wire = frame.encode().unwrap();
        let back = IngestFrame::decode(&wire[frame::HEADER_SIZE..]).unwrap();
        assert_eq!(back, frame);
    }
}

#[test]
fn decode_rejects_an_unknown_wire_version() {
    let mut log = LogFrame::new(1, "src", Level::Info, "m");
    log.v = 99;
    let frame = IngestFrame::Log(log);
    let wire = frame.encode().unwrap();
    let err = IngestFrame::decode(&wire[frame::HEADER_SIZE..]).unwrap_err();
    assert!(matches!(err, LogdError::Version { got: 99, ours: 1 }));
}

#[test]
fn level_integer_encoding_round_trips() {
    for lvl in [
        Level::Trace,
        Level::Debug,
        Level::Info,
        Level::Warn,
        Level::Error,
    ] {
        assert_eq!(Level::from_u8(lvl.as_u8()), lvl);
    }
    // Out-of-range integers clamp to the most severe level.
    assert_eq!(Level::from_u8(200), Level::Error);
}

#[test]
fn query_response_envelope_serializes_with_the_expected_shape() {
    let resp = QueryResponse {
        data: vec![1u32, 2, 3],
        page: Page {
            next_cursor: Some("abc".to_string()),
            count: 3,
        },
        meta: Meta {
            source: "local".to_string(),
            v: ENVELOPE_VERSION,
            ts: 42,
            db_lag_ms: 0,
        },
    };
    let json = serde_json::to_value(&resp).unwrap();
    assert_eq!(json["data"], serde_json::json!([1, 2, 3]));
    assert_eq!(json["page"]["next_cursor"], "abc");
    assert_eq!(json["page"]["count"], 3);
    assert_eq!(json["meta"]["source"], "local");
    assert_eq!(json["meta"]["v"], 1);
    assert_eq!(json["meta"]["db_lag_ms"], 0);
}

#[test]
fn error_envelope_shape() {
    let err = ErrorEnvelope::new("bad_request", "unknown filter");
    let json = serde_json::to_value(&err).unwrap();
    assert_eq!(json["error"]["code"], "bad_request");
    assert_eq!(json["error"]["message"], "unknown filter");
}

#[test]
fn is_secret_key_matches_suffix_and_exact_case_insensitively() {
    assert!(is_secret_key("api_key"));
    assert!(is_secret_key("KEY"));
    assert!(is_secret_key("pairing_code"));
    assert!(is_secret_key("token"));
    assert!(is_secret_key("Password"));
    assert!(is_secret_key("device_secret"));
    // Not secret-bearing.
    assert!(!is_secret_key("device_id"));
    assert!(!is_secret_key("keyboard")); // "key" is a prefix, not a suffix
    assert!(!is_secret_key("message"));
}

#[test]
fn redact_leaves_non_secret_and_empty_and_sentinel_values_untouched() {
    // Non-secret key: unchanged.
    assert_eq!(redact("device_id", "abc123"), "abc123");
    // Empty value: unchanged even for a secret key.
    assert_eq!(redact("api_key", ""), "");
    // Already redacted: idempotent.
    let once = redact("api_key", "ABCDEFGHIJ1234567890");
    assert_eq!(redact("api_key", &once), once);
}

/// Byte-for-byte parity with the agent's structured-logging redaction (the
/// Python tests carry the same vectors). Nothing of the value survives but
/// its length, so none of these can be reversed by hashing candidates.
#[test]
fn redact_parity_vectors() {
    let vectors: &[(&str, &str, &str)] = &[
        ("api_key", "ABCDEFGHIJ1234567890", "redacted:len=20"),
        ("pairing_code", "999888", "redacted:len=6"),
        ("token", "tok_supersecretvalue", "redacted:len=20"),
        ("password", "hunter2", "redacted:len=7"),
        ("secret", "s", "redacted:len=1"),
        ("device_secret", "0xDEADBEEFCAFE", "redacted:len=14"),
    ];
    for (key, value, expected) in vectors {
        assert_eq!(&redact(key, value), expected, "key={key} value={value}");
        // Idempotent: feeding the redacted output back yields the same string.
        assert_eq!(&redact(key, expected), expected, "idempotent key={key}");
    }
}

#[test]
fn redact_map_redacts_only_secret_string_fields() {
    let mut fields = mk_fields(&[("api_key", "ABCDEFGHIJ1234567890"), ("device_id", "abc123")]);
    fields.insert("attempt".to_string(), MpVal::from(7u64)); // non-string, skipped
    let changed = redact_map(&mut fields);
    assert!(changed, "redaction changed the secret field");
    assert_eq!(
        fields.get("api_key").and_then(|v| v.as_str()),
        Some("redacted:len=20")
    );
    assert_eq!(
        fields.get("device_id").and_then(|v| v.as_str()),
        Some("abc123")
    );
    assert_eq!(fields.get("attempt").and_then(|v| v.as_u64()), Some(7));
}

#[test]
fn redact_map_reports_no_change_when_nothing_is_secret() {
    // A map with only non-secret and already-redacted values reports no
    // change, so a caller can record an accurate redaction flag on the row.
    let mut fields = mk_fields(&[("device_id", "abc123"), ("api_key", "redacted:len=20")]);
    let changed = redact_map(&mut fields);
    assert!(!changed, "no value should change");
    assert_eq!(
        fields.get("api_key").and_then(|v| v.as_str()),
        Some("redacted:len=20")
    );
}

#[test]
fn frame_level_redaction_is_applied_through_ingest() {
    let mut log = LogFrame::new(1, "src", Level::Info, "m");
    log.fields.insert(
        "session_token".to_string(),
        MpVal::from("tok_supersecretvalue"),
    );
    let mut frame = IngestFrame::Log(log);
    frame.redact();
    if let IngestFrame::Log(l) = &frame {
        assert_eq!(
            l.fields.get("session_token").and_then(|v| v.as_str()),
            Some("redacted:len=20")
        );
    } else {
        panic!("expected log frame");
    }
}

#[test]
fn sync_request_and_response_round_trip() {
    // A fully-populated request round-trips through JSON unchanged.
    let req = SyncRequest {
        session: Some(7),
        from_us: Some(1_700_000_000_000_000),
        to_us: Some(1_700_000_600_000_000),
        tables: vec![SyncTable::Logs, SyncTable::Events],
    };
    let json = serde_json::to_value(&req).unwrap();
    assert_eq!(json["session"], 7);
    assert_eq!(json["tables"], serde_json::json!(["logs", "events"]));
    let back: SyncRequest = serde_json::from_value(json).unwrap();
    assert_eq!(back, req);

    // An empty request omits every optional field, and tables_or_all expands
    // the absent selector to all four tables.
    let empty = SyncRequest::default();
    let ejson = serde_json::to_value(&empty).unwrap();
    assert_eq!(ejson, serde_json::json!({}));
    assert_eq!(
        empty.tables_or_all(),
        vec![
            SyncTable::Logs,
            SyncTable::Metrics,
            SyncTable::Events,
            SyncTable::Hw
        ]
    );
    // A populated selector is returned as listed.
    assert_eq!(
        req.tables_or_all(),
        vec![SyncTable::Logs, SyncTable::Events]
    );

    // The response round-trips with its two per-table maps.
    let resp = SyncResponse {
        marked: BTreeMap::from([("logs".to_string(), 142)]),
        unsynced_after: BTreeMap::from([
            ("logs".to_string(), 0),
            ("metrics".to_string(), 318),
            ("events".to_string(), 12),
            ("hw".to_string(), 90),
        ]),
    };
    let rjson = serde_json::to_value(&resp).unwrap();
    assert_eq!(rjson["marked"]["logs"], 142);
    assert_eq!(rjson["unsynced_after"]["metrics"], 318);
    let rback: SyncResponse = serde_json::from_value(rjson).unwrap();
    assert_eq!(rback, resp);

    // Each table variant maps to its lowercase table name.
    for t in SyncTable::ALL {
        let s = serde_json::to_value(t).unwrap();
        assert_eq!(s.as_str().unwrap(), t.as_str());
    }
}
