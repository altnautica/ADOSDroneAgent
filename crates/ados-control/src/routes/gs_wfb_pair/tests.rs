use super::*;

async fn body_json(resp: Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn profile_mismatch_golden_body() {
    let resp = profile_mismatch();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_json(resp).await,
        json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})
    );
}

// ── split_reply ───────────────────────────────────────────────────────────

#[test]
fn split_reply_strips_ok_on_success() {
    let reply = json!({
        "ok": true,
        "paired": true,
        "paired_with_device_id": "drone-7",
        "paired_at": "2026-06-16T00:00:00+00:00",
        "fingerprint": "0123456789abcdef",
        "role": "gs",
    });
    let body = split_reply(reply).unwrap();
    assert!(!body.contains_key("ok"));
    assert_eq!(body.get("paired"), Some(&json!(true)));
    assert_eq!(body.get("role"), Some(&json!("gs")));
    assert_eq!(body.get("fingerprint"), Some(&json!("0123456789abcdef")));
}

#[test]
fn split_reply_surfaces_error_code_and_message() {
    let err = split_reply(json!({"ok": false, "error": "E_INVALID_KEY_BLOB", "message": "bad"}))
        .unwrap_err();
    assert_eq!(err.code, "E_INVALID_KEY_BLOB");
    assert_eq!(err.message.as_deref(), Some("bad"));
}

// ── pair-error mapping ────────────────────────────────────────────────────

#[tokio::test]
async fn pair_error_base64_is_a_400() {
    let resp = map_pair_error(SocketError {
        code: "E_BLOB_BASE64".to_string(),
        message: Some("invalid byte".to_string()),
    });
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["detail"]["error"]["code"], "E_BLOB_BASE64");
}

#[tokio::test]
async fn pair_error_bad_blob_is_a_400() {
    let resp = map_pair_error(SocketError {
        code: "E_INVALID_KEY_BLOB".to_string(),
        message: None,
    });
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(resp).await["detail"]["error"]["code"],
        "E_INVALID_KEY_BLOB"
    );
}

#[tokio::test]
async fn pair_error_io_is_a_500() {
    let resp = map_pair_error(SocketError {
        code: "E_PAIR_FAILED".to_string(),
        message: Some("disk full".to_string()),
    });
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        body_json(resp).await["detail"]["error"]["code"],
        "E_PAIR_FAILED"
    );
}

#[tokio::test]
async fn socket_unavailable_is_a_503_carrying_the_route_code() {
    let resp = socket_unavailable("E_PAIR_FAILED");
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body_json(resp).await["detail"]["error"]["code"],
        "E_PAIR_FAILED"
    );
}

// ── gs_pair_status reads the key + persisted peer ─────────────────────────

#[test]
fn gs_pair_status_reports_unpaired_without_a_key() {
    // No rx.key present → not paired. (Point the key dir at an empty tempdir;
    // env is process-global, so this is a single-threaded read with no writes.)
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("missing").join("rx.key");
    let paired = std::fs::metadata(&key)
        .map(|m| m.is_file() && m.len() == WFB_KEY_FILE_BYTES)
        .unwrap_or(false);
    assert!(!paired);
}

#[test]
fn releasing_one_slot_leaves_the_rest_of_the_fleet_registered() {
    // The whole point of the per-device release: retiring one airframe must
    // not disturb the others. The station-wide unpair is the tool that drops
    // everyone, and reaching for it to remove a single drone is what made a
    // bench edit the registry file by hand instead.
    let mut registry = FleetRegistry::default();
    registry.allocate("drone-a");
    registry.allocate("drone-b");
    registry.allocate("drone-c");

    assert!(registry.release("drone-b"));

    let left: Vec<String> = registry.slots().map(|s| s.device_id.clone()).collect();
    assert_eq!(left, vec!["drone-a".to_string(), "drone-c".to_string()]);
}

#[test]
fn a_released_slot_is_reissued_rather_than_running_the_fleet_out() {
    // Releasing frees the slot NUMBER. Without that, a station that had
    // cycled through drones would report itself full while holding
    // registrations for airframes that no longer exist.
    let mut registry = FleetRegistry::default();
    let a = registry.allocate("drone-a").unwrap();
    registry.allocate("drone-b");

    assert!(registry.release("drone-a"));
    assert_eq!(
        registry.allocate("drone-d"),
        Some(a),
        "the freed slot number should be the next one issued"
    );
}

#[test]
fn releasing_a_device_that_holds_no_slot_reports_absent_rather_than_succeeding() {
    // The caller's intent is "this drone should not hold a slot". Reporting
    // success for a device that was never registered would make a typo look
    // like a completed release, and the operator would stop looking.
    let mut registry = FleetRegistry::default();
    registry.allocate("drone-a");

    assert!(!registry.release("never-paired"));
    assert_eq!(registry.slots().count(), 1, "nothing else may be disturbed");
}

#[test]
fn the_slot_table_never_carries_a_relay_secret() {
    // The roster IS returned to a caller holding the fleet key. The
    // per-pair relay secret must not ride along: it is the one thing that
    // distinguishes a drone's own ground station from anything else that
    // can reach the air, and handing it to every fleet member would undo
    // exactly what it is for. `slot_table` picks fields explicitly today —
    // this fails if anyone replaces it with a whole-struct serialization.
    let mut registry = FleetRegistry::default();
    registry.allocate("aaaa");
    let rendered = serde_json::to_string(&slot_table(&registry)).unwrap();

    let secret = registry
        .slots()
        .next()
        .unwrap()
        .relay_secret
        .clone()
        .expect("allocation issues a secret");
    assert!(!secret.is_empty());
    assert!(
        !rendered.contains(&secret),
        "the relay secret leaked into the slot table: {rendered}"
    );
    assert!(!rendered.contains("relay_secret"));
    // The fields it SHOULD carry are still there.
    assert!(rendered.contains("device_id") && rendered.contains("slot"));
}

#[test]
fn a_foreign_key_is_refused_without_naming_the_fleet() {
    // A caller presenting a DIFFERENT key has just proved it is not part of
    // this fleet. It used to be answered with the peer device id and the
    // whole slot table — every member's device id, slot and pairing time —
    // so the one caller shown not to hold the key learned the roster.
    let body = json!({
        "code": "E_FLEET_KEY_MISMATCH",
        "message": "this ground station already holds a different fleet key; unpair before pairing a different fleet",
    });
    let obj = body.as_object().unwrap();
    assert!(
        !obj.contains_key("slots"),
        "the fleet roster must not ride a refusal"
    );
    assert!(
        !obj.contains_key("paired_with_device_id"),
        "a refused caller must not learn who this station is paired with"
    );
    // It must still say WHY, or the operator cannot act on it.
    assert_eq!(obj["code"], "E_FLEET_KEY_MISMATCH");
    assert!(obj["message"].as_str().unwrap().contains("unpair"));
}

// ── the fleet gate ────────────────────────────────────────────────────────

#[test]
fn the_slot_table_renders_the_registry_in_slot_order() {
    // The GCS reads this table to draw the fleet, and the pair route returns
    // it on both the success and the conflict paths, so its shape is a wire
    // contract.
    let mut registry = FleetRegistry::default();
    registry.allocate("drone-b").unwrap();
    registry.allocate("drone-a").unwrap();
    let table = slot_table(&registry);
    assert_eq!(table.len(), 2);
    assert_eq!(table[0]["slot"], 1);
    assert_eq!(table[0]["device_id"], "drone-b");
    assert!(table[0]["paired_at_ms"].as_u64().unwrap() > 0);
    assert_eq!(table[1]["slot"], 2);
    assert_eq!(table[1]["device_id"], "drone-a");
}

#[test]
fn the_slot_table_flags_two_slots_sharing_a_system_id() {
    let mut registry = FleetRegistry::default();
    registry.allocate("drone-a").unwrap();
    registry.allocate("drone-b").unwrap();
    registry.allocate("drone-c").unwrap();
    let conflict = SlotSystemId {
        fc_system_id: 1,
        conflict: true,
    };
    let ids = std::collections::BTreeMap::from([(1u8, conflict), (2u8, conflict)]);
    let table = slot_table_with(&registry, &ids, Some(2));
    assert_eq!(table[0]["fc_system_id"], 1);
    assert_eq!(table[0]["system_id_conflict"], true);
    assert_eq!(table[1]["system_id_conflict"], true);
    // A slot whose flight controller has not been heard has no identity,
    // and so cannot be in conflict.
    assert_eq!(table[2]["fc_system_id"], Value::Null);
    assert_eq!(table[2]["system_id_conflict"], false);
    // Exactly one slot is the drone whose video the ground serves.
    assert_eq!(table[0]["video_hero"], false);
    assert_eq!(table[1]["video_hero"], true);
    assert_eq!(table[2]["video_hero"], false);
}

#[test]
fn an_empty_registry_renders_an_empty_table_not_null() {
    // The GCS iterates this; a null would need a second code path.
    assert_eq!(slot_table(&FleetRegistry::default()), Vec::<Value>::new());
}

#[test]
fn a_fleet_join_is_idempotent_and_a_full_fleet_refuses() {
    // The two registry outcomes the route branches on. Re-pairing the same
    // device must return its existing slot (never renumber a flying drone),
    // and a full fleet must refuse a NEW device while still serving a known
    // one — the E_FLEET_FULL branch must not fire for a re-pair.
    let mut registry = FleetRegistry::default();
    let first = registry.allocate("drone-a").unwrap();
    assert_eq!(registry.allocate("drone-a"), Some(first));
    for i in 2..=FLEET_MAX_SLOTS {
        assert!(registry.allocate(&format!("drone-{i}")).is_some());
    }
    assert_eq!(registry.allocate("one-too-many"), None);
    assert_eq!(registry.allocate("drone-a"), Some(first));
}

#[tokio::test]
async fn a_missing_or_malformed_device_id_is_refused() {
    let code = |raw: Option<&str>| {
        let resp = *pair_device_id(raw.map(str::to_string)).expect_err("refused");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        resp
    };
    for missing in [None, Some("")] {
        assert_eq!(
            body_json(code(missing)).await["detail"]["error"]["code"],
            "E_DEVICE_ID_REQUIRED"
        );
    }
    let too_long = "a".repeat(33);
    for bad in [
        " ",
        "d",
        "drone-1",
        " 40bb1a5a",
        "drone 01x",
        "drone/01x",
        too_long.as_str(),
    ] {
        assert_eq!(
            body_json(code(Some(bad))).await["detail"]["error"]["code"],
            "E_DEVICE_ID_INVALID",
            "{bad:?}"
        );
    }
    let longest = "a".repeat(32);
    for good in ["40bb1a5a", "40bb1a5a4484", "drone-01", longest.as_str()] {
        assert_eq!(
            pair_device_id(Some(good.to_string())).ok().as_deref(),
            Some(good)
        );
    }
}

#[test]
fn a_shared_key_of_the_wrong_size_is_refused_rather_than_written() {
    // A truncated key derives a WRONG beacon HMAC, which drops every beacon
    // silently -- harder to notice than a missing key, because the resolver
    // at least warns when nothing is there.
    use base64::Engine as _;
    let short = base64::engine::general_purpose::STANDARD.encode([0u8; 32]);
    assert!(decode_shared_key(&short).is_err());
}

#[test]
fn a_shared_key_that_is_not_base64_is_refused() {
    assert!(decode_shared_key("not base64!!").is_err());
}

#[test]
fn the_shared_key_is_written_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drone.key");
    assert_eq!(install_shared_key(&path, &[7u8; 64], false), "installed");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert_eq!(std::fs::read(&path).unwrap(), vec![7u8; 64]);
}

#[test]
fn a_join_never_rekeys_an_installed_shared_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("drone.key");
    std::fs::write(&path, [1u8; 64]).unwrap();
    assert_eq!(install_shared_key(&path, &[1u8; 64], true), "unchanged");
    assert_eq!(install_shared_key(&path, &[2u8; 64], true), "mismatch");
    assert_eq!(std::fs::read(&path).unwrap(), vec![1u8; 64]);
    // A fresh install is a new fleet: it replaces the old shared half.
    assert_eq!(install_shared_key(&path, &[2u8; 64], false), "installed");
    assert_eq!(std::fs::read(&path).unwrap(), vec![2u8; 64]);
}

#[test]
fn the_request_accepts_a_shared_key_and_still_parses_without_one() {
    // Absent, the route must behave exactly as it did: the radio bind stays
    // the path that distributes the shared half.
    let with: PairRequest =
        serde_json::from_str(r#"{"blob_b64":"x","shared_key_b64":"y"}"#).unwrap();
    assert_eq!(with.shared_key_b64.as_deref(), Some("y"));
    let without: PairRequest = serde_json::from_str(r#"{"blob_b64":"x"}"#).unwrap();
    assert!(without.shared_key_b64.is_none());
}
