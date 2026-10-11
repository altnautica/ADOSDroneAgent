use super::*;

/// Read a response body as JSON.
async fn body_json(resp: Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Two RTL USB radios under a fake sysfs root pass the relay gate; one
/// radio is refused with `E_MESH_NOT_CAPABLE`; `direct` never probes.
#[cfg(unix)]
#[tokio::test]
async fn role_relay_passes_on_two_radio_fake_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let net = root.join("net");
    for (i, name) in ["wlan1", "wlan2"].iter().enumerate() {
        let usb = root.join("devices").join(format!("1-{i}"));
        let intf = usb.join(format!("1-{i}:1.0"));
        let drv = root.join("drivers").join("rtl88x2eu");
        std::fs::create_dir_all(&intf).unwrap();
        std::fs::create_dir_all(&drv).unwrap();
        std::fs::write(usb.join("idVendor"), "0bda\n").unwrap();
        std::fs::write(usb.join("idProduct"), "a81a\n").unwrap();
        std::os::unix::fs::symlink(&drv, intf.join("driver")).unwrap();
        let iface = net.join(name);
        std::fs::create_dir_all(iface.join("phy80211")).unwrap();
        std::os::unix::fs::symlink(&intf, iface.join("device")).unwrap();
    }
    let probe = || ados_protocol::netif::mesh_capable_in(&net);
    assert!(mesh_capability_gate("relay", probe).is_none());
    assert!(mesh_capability_gate("receiver", probe).is_none());

    std::fs::remove_dir_all(net.join("wlan2")).unwrap();
    let refused = mesh_capability_gate("relay", probe).unwrap();
    assert_eq!(refused.status(), StatusCode::CONFLICT);
    assert_eq!(
        body_json(refused).await,
        json!({"detail": {"error": {"code": "E_MESH_NOT_CAPABLE"}}})
    );
    assert!(mesh_capability_gate("direct", || panic!("direct never probes")).is_none());
}

// ── profile gate ─────────────────────────────────────────────────────────

#[tokio::test]
async fn profile_mismatch_golden_body() {
    // The body every ground-station write returns on a drone, pinned as the
    // golden fixture for the conformance harness's off-a-drone diff.
    let resp = profile_mismatch();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_json(resp).await,
        json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})
    );
}

#[tokio::test]
async fn not_in_mesh_body() {
    let resp = nested_detail(StatusCode::NOT_FOUND, json!({"code": "E_NOT_IN_MESH"}));
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_json(resp).await,
        json!({"detail": {"error": {"code": "E_NOT_IN_MESH"}}})
    );
}

// ── strip_ok ──────────────────────────────────────────────────────────────

#[test]
fn strip_ok_removes_the_transport_flag() {
    let reply = json!({
        "ok": true,
        "role": "relay",
        "previous": "direct",
        "units_started": ["ados-batman.service", "ados-wfb-relay.service"],
        "units_stopped": [],
        "ts_ms": 1234,
        "noop": false,
    });
    let body = strip_ok(reply).unwrap();
    assert!(!body.contains_key("ok"));
    assert_eq!(body.get("role"), Some(&json!("relay")));
    assert_eq!(body.get("previous"), Some(&json!("direct")));
    assert_eq!(body.get("noop"), Some(&json!(false)));
    assert_eq!(body.get("ts_ms"), Some(&json!(1234)));
}

#[test]
fn strip_ok_surfaces_an_error_reply() {
    let err = strip_ok(json!({"ok": false, "error": "E_BATCTL_UNAVAILABLE"})).unwrap_err();
    assert_eq!(err.code, "E_BATCTL_UNAVAILABLE");
    assert!(err.message.is_none());
    let err2 =
        strip_ok(json!({"ok": false, "error": "E_INVALID_ROLE", "message": "bad"})).unwrap_err();
    assert_eq!(err2.code, "E_INVALID_ROLE");
    assert_eq!(err2.message.as_deref(), Some("bad"));
}

// ── invalid-role 400 ──────────────────────────────────────────────────────

#[tokio::test]
async fn socket_error_maps_to_a_nested_400() {
    let resp = socket_error_to_response(SocketError {
        code: "E_INVALID_ROLE".to_string(),
        message: Some(
            "role must be one of [\"direct\", \"relay\", \"receiver\"], got \"x\"".to_string(),
        ),
    });
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body["detail"]["error"]["code"], "E_INVALID_ROLE");
    assert!(body["detail"]["error"]["message"].as_str().is_some());
}

#[tokio::test]
async fn supervisor_unavailable_is_a_503() {
    let resp = supervisor_unavailable();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = body_json(resp).await;
    assert_eq!(body["detail"]["error"]["code"], "E_SUPERVISOR_UNAVAILABLE");
}

// ── mesh/config merge ─────────────────────────────────────────────────────

#[tokio::test]
async fn mesh_config_merges_supplied_fields_and_keeps_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(
            &cfg,
            "agent:\n  name: gs-1\nground_station:\n  role: direct\n  mesh:\n    mesh_id: site-a\n    carrier: 802.11s\n    channel: 1\n",
        )
        .unwrap();

    let update = MeshConfigUpdate {
        mesh_id: Some("site-b".to_string()),
        carrier: Some("ibss".to_string()),
        channel: None,
    };
    let resp = put_mesh_config_at(&cfg, &update);
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    // mesh_id + carrier take the request; channel keeps the on-disk value.
    assert_eq!(
        body,
        json!({
            "mesh_id": "site-b",
            "carrier": "ibss",
            "channel": 1,
            "applied": true,
        })
    );

    // The on-disk merge applied the supplied fields, kept the unset one, and
    // preserved the unrelated keys.
    let parsed: serde_norway::Value =
        serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    let mesh = parsed
        .get("ground_station")
        .and_then(|g| g.get("mesh"))
        .unwrap();
    assert_eq!(mesh.get("mesh_id").and_then(|v| v.as_str()), Some("site-b"));
    assert_eq!(mesh.get("carrier").and_then(|v| v.as_str()), Some("ibss"));
    assert_eq!(mesh.get("channel").and_then(norway_to_i64), Some(1));
    // The unrelated agent.name + the ground_station.role survived.
    assert_eq!(
        parsed
            .get("agent")
            .and_then(|a| a.get("name"))
            .and_then(|n| n.as_str()),
        Some("gs-1")
    );
    assert_eq!(
        parsed
            .get("ground_station")
            .and_then(|g| g.get("role"))
            .and_then(|r| r.as_str()),
        Some("direct")
    );
}

#[tokio::test]
async fn mesh_config_empty_body_is_a_noop_with_defaults() {
    // An empty body (all fields null) is applied=false and echoes the existing
    // values (here: the config-model defaults, since the file is empty).
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    let resp = put_mesh_config_at(&cfg, &MeshConfigUpdate::default());
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body,
        json!({
            "mesh_id": Value::Null,
            "carrier": "802.11s",
            "channel": 1,
            "applied": false,
        })
    );
    // A no-op write leaves no file behind (the merge only writes when applied).
    assert!(!cfg.exists());
}

#[tokio::test]
async fn mesh_config_no_existing_section_uses_model_defaults() {
    // A file with no ground_station.mesh section → only the supplied field is
    // written; the unset fields resolve to the config-model defaults.
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(&cfg, "agent:\n  name: x\n").unwrap();
    let update = MeshConfigUpdate {
        mesh_id: None,
        carrier: None,
        channel: Some(6),
    };
    let resp = put_mesh_config_at(&cfg, &update);
    let body = body_json(resp).await;
    assert_eq!(
        body,
        json!({
            "mesh_id": Value::Null,
            "carrier": "802.11s",
            "channel": 6,
            "applied": true,
        })
    );
    let parsed: serde_norway::Value =
        serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    let mesh = parsed
        .get("ground_station")
        .and_then(|g| g.get("mesh"))
        .unwrap();
    assert_eq!(mesh.get("channel").and_then(norway_to_i64), Some(6));
    assert!(mesh.get("mesh_id").is_none());
    assert!(mesh.get("carrier").is_none());
}

#[tokio::test]
async fn mesh_config_rejects_out_of_range_fields_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    let original = "ground_station:\n  mesh:\n    carrier: 802.11s\n    channel: 6\n";
    std::fs::write(&cfg, original).unwrap();
    let bad = [
        MeshConfigUpdate {
            carrier: Some("wds".into()),
            ..Default::default()
        },
        MeshConfigUpdate {
            channel: Some(0),
            ..Default::default()
        },
        MeshConfigUpdate {
            channel: Some(14),
            ..Default::default()
        },
        MeshConfigUpdate {
            mesh_id: Some(String::new()),
            ..Default::default()
        },
        MeshConfigUpdate {
            mesh_id: Some("a".repeat(33)),
            ..Default::default()
        },
        MeshConfigUpdate {
            mesh_id: Some("site b".into()),
            ..Default::default()
        },
    ];
    for update in &bad {
        let resp = put_mesh_config_at(&cfg, update);
        assert_eq!(
            resp.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{update:?}"
        );
        let body = body_json(resp).await;
        assert_eq!(
            body["detail"]["error"]["code"],
            json!("E_INVALID_MESH_CONFIG")
        );
    }
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original);
}

// ── ground_station.role best-effort persist ───────────────────────────────

#[test]
fn merge_ground_station_role_persists_and_keeps_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(
        &cfg,
        "agent:\n  name: gs-1\nground_station:\n  role: direct\n",
    )
    .unwrap();
    merge_ground_station_role(&cfg, "relay").unwrap();
    let parsed: serde_norway::Value =
        serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    assert_eq!(
        parsed
            .get("ground_station")
            .and_then(|g| g.get("role"))
            .and_then(|r| r.as_str()),
        Some("relay")
    );
    assert_eq!(
        parsed
            .get("agent")
            .and_then(|a| a.get("name"))
            .and_then(|n| n.as_str()),
        Some("gs-1")
    );
}

// ── existing_mesh projection ──────────────────────────────────────────────

#[test]
fn existing_mesh_reads_typed_fields_and_defaults_missing() {
    let data: serde_norway::Value =
        serde_norway::from_str("ground_station:\n  mesh:\n    mesh_id: site-a\n    channel: 11\n")
            .unwrap();
    let e = existing_mesh(data.as_mapping().unwrap());
    assert_eq!(e.mesh_id, json!("site-a"));
    assert_eq!(e.carrier, None);
    assert_eq!(e.channel, Some(11));
    // No section at all → null mesh_id + no carrier/channel.
    let empty: serde_norway::Value = serde_norway::from_str("agent:\n  name: x\n").unwrap();
    let e2 = existing_mesh(empty.as_mapping().unwrap());
    assert_eq!(e2.mesh_id, Value::Null);
    assert!(e2.carrier.is_none() && e2.channel.is_none());
}
