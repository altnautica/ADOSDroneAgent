use super::*;

#[test]
fn validate_rejects_non_64() {
    assert!(validate_blob(&[0u8; 64]).is_ok());
    assert!(validate_blob(&[0u8; 63]).is_err());
    assert!(validate_blob(&[0u8; 65]).is_err());
    assert!(validate_blob(&[]).is_err());
}

#[tokio::test]
async fn apply_keypair_rejects_a_bad_blob_before_touching_any_unit() {
    // The blob is validated up front, before the apply task is spawned: a
    // bad-length blob returns Err without ever stopping a unit or writing a
    // key, so the atomic stop→write→start sequence only runs once a real key
    // blob has arrived (the precondition the cancellation-safe spawn relies on).
    let err = apply_keypair(
        crate::process_manager::select(),
        &[0u8; 10],
        BindRole::Drone,
        None,
        None,
    )
    .await
    .expect_err("a 10-byte blob must be rejected");
    assert!(
        err.contains("expected"),
        "error names the size mismatch: {err}"
    );
}

#[test]
fn write_atomic_sets_mode_and_content() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("sub").join("tx.key");
    write_atomic(&p, &[7u8; 64], 0o600).unwrap();
    assert_eq!(std::fs::read(&p).unwrap(), vec![7u8; 64]);
    let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    // No leftover temp file.
    let names: Vec<_> = std::fs::read_dir(dir.path().join("sub"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names.len(), 1, "{names:?}");
}

#[test]
fn fingerprint_is_16_hex_deterministic_and_sensitive() {
    let dir = tempfile::tempdir().unwrap();
    let mut key_a = [0u8; 64];
    key_a[32..].copy_from_slice(&[1u8; 32]); // pub half = all 1s
    let mut key_b = [0u8; 64];
    key_b[32..].copy_from_slice(&[2u8; 32]); // pub half = all 2s
    let pa = dir.path().join("a.key");
    let pb = dir.path().join("b.key");
    write_atomic(&pa, &key_a, 0o600).unwrap();
    write_atomic(&pb, &key_b, 0o600).unwrap();
    let fa = read_public_fingerprint(&pa).unwrap();
    let fb = read_public_fingerprint(&pb).unwrap();
    assert_eq!(fa.len(), 16);
    assert!(fa
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
    assert_eq!(fa, read_public_fingerprint(&pa).unwrap()); // deterministic
    assert_ne!(fa, fb); // sensitive to the pub half
                        // A wrong-sized file is rejected.
    let pbad = dir.path().join("bad.key");
    write_atomic(&pbad, &[0u8; 10], 0o600).unwrap();
    assert!(read_public_fingerprint(&pbad).is_err());
}

#[test]
fn apply_fields_drone_sets_then_clears() {
    let mut root = Mapping::new();
    apply_pair_fields(
        &mut root,
        BindRole::Drone,
        Some("dev-123"),
        Some("2026-05-29T00:00:00+00:00"),
        Some(false),
        Some(FleetIdentity {
            fleet_id: 4,
            fleet_slot: 7,
        }),
    );
    let wfb = root
        .get("video")
        .and_then(Value::as_mapping)
        .and_then(|m| m.get("wfb"))
        .and_then(Value::as_mapping)
        .unwrap();
    assert_eq!(
        wfb.get("paired_with_device_id").unwrap(),
        &Value::String("dev-123".into())
    );
    assert_eq!(
        wfb.get("paired_at").unwrap(),
        &Value::String("2026-05-29T00:00:00+00:00".into())
    );
    assert_eq!(wfb.get("auto_pair_enabled").unwrap(), &Value::Bool(false));
    assert_eq!(wfb.get("fleet_id").unwrap(), &Value::Number(4.into()));
    assert_eq!(wfb.get("fleet_slot").unwrap(), &Value::Number(7.into()));
    // Drone never writes the ground_station mirror.
    assert!(root.get("ground_station").is_none());

    // Clearing (unpair shape): peer None + paired_at None → keys removed.
    apply_pair_fields(&mut root, BindRole::Drone, None, None, Some(true), None);
    let wfb = root
        .get("video")
        .and_then(Value::as_mapping)
        .and_then(|m| m.get("wfb"))
        .and_then(Value::as_mapping)
        .unwrap();
    assert!(wfb.get("paired_with_device_id").is_none());
    assert!(wfb.get("paired_at").is_none());
    assert_eq!(wfb.get("auto_pair_enabled").unwrap(), &Value::Bool(true));
}

#[test]
fn a_bind_with_no_assignment_leaves_an_existing_slot_intact() {
    // The failure this guards: a re-pair that carries no fleet assignment
    // must not strip a drone's slot. `fleet_slot: 0` on a drone profile is
    // the ground station's slot, which fails the drone-side identity gate —
    // so a cleared slot parks the radio and the drone silently stops
    // radiating on the next restart.
    let mut root = Mapping::new();
    apply_pair_fields(
        &mut root,
        BindRole::Drone,
        Some("dev-1"),
        Some("ts"),
        Some(false),
        Some(FleetIdentity {
            fleet_id: 2,
            fleet_slot: 5,
        }),
    );
    apply_pair_fields(
        &mut root,
        BindRole::Drone,
        Some("dev-1"),
        Some("ts-2"),
        Some(false),
        None,
    );
    let wfb = root
        .get("video")
        .and_then(Value::as_mapping)
        .and_then(|m| m.get("wfb"))
        .and_then(Value::as_mapping)
        .unwrap();
    assert_eq!(wfb.get("fleet_id").unwrap(), &Value::Number(2.into()));
    assert_eq!(wfb.get("fleet_slot").unwrap(), &Value::Number(5.into()));
}

#[test]
fn a_reassignment_overwrites_the_previous_slot() {
    // The other half of the rule above: an assignment that IS supplied wins,
    // so a drone moved to a new slot re-keys instead of keeping the old one
    // and colliding with whoever now holds it.
    let mut root = Mapping::new();
    for slot in [5u8, 9u8] {
        apply_pair_fields(
            &mut root,
            BindRole::Drone,
            Some("dev-1"),
            Some("ts"),
            Some(false),
            Some(FleetIdentity {
                fleet_id: 2,
                fleet_slot: slot,
            }),
        );
    }
    let wfb = root
        .get("video")
        .and_then(Value::as_mapping)
        .and_then(|m| m.get("wfb"))
        .and_then(Value::as_mapping)
        .unwrap();
    assert_eq!(wfb.get("fleet_slot").unwrap(), &Value::Number(9.into()));
}

#[test]
fn apply_fields_gs_mirrors_ground_station() {
    let mut root = Mapping::new();
    apply_pair_fields(
        &mut root,
        BindRole::Gs,
        Some("drone-9"),
        Some("ts"),
        Some(false),
        Some(FleetIdentity {
            fleet_id: 1,
            fleet_slot: 0,
        }),
    );
    let gs = root
        .get("ground_station")
        .and_then(Value::as_mapping)
        .unwrap();
    assert_eq!(
        gs.get("paired_drone_id").unwrap(),
        &Value::String("drone-9".into())
    );
    assert_eq!(gs.get("paired_at").unwrap(), &Value::String("ts".into()));
    // A ground station always takes slot 0; it is not an aircraft.
    let wfb = root
        .get("video")
        .and_then(Value::as_mapping)
        .and_then(|m| m.get("wfb"))
        .and_then(Value::as_mapping)
        .unwrap();
    assert_eq!(wfb.get("fleet_slot").unwrap(), &Value::Number(0.into()));
}

#[test]
fn persist_round_trips_via_serde_on_dev_host() {
    // On a non-Linux dev host the euid/flock gates are cfg'd out, so this
    // exercises load → mutate → atomic-write → reload end-to-end.
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(
        &cfg,
        "video:\n  wfb:\n    channel: 149\nagent:\n  profile: drone\n",
    )
    .unwrap();
    let lock = dir.path().join("config.yaml.lock");
    let ok = persist_pair_state(
        &cfg,
        &lock,
        BindRole::Drone,
        Some("peer-1"),
        Some("ts-1"),
        Some(false),
        Some(FleetIdentity {
            fleet_id: 3,
            fleet_slot: 11,
        }),
    );
    // On Linux as non-root this returns false; on the dev host it writes.
    if cfg!(target_os = "linux") {
        return;
    }
    assert!(ok);
    let reloaded = try_load_config_mapping(&cfg).unwrap();
    let wfb = reloaded
        .get("video")
        .and_then(Value::as_mapping)
        .and_then(|m| m.get("wfb"))
        .and_then(Value::as_mapping)
        .unwrap();
    assert_eq!(
        wfb.get("paired_with_device_id").unwrap(),
        &Value::String("peer-1".into())
    );
    assert_eq!(wfb.get("auto_pair_enabled").unwrap(), &Value::Bool(false));
    // Pre-existing keys preserved.
    assert_eq!(wfb.get("channel").unwrap(), &Value::Number(149.into()));
    assert!(reloaded.get("agent").is_some());
}

#[test]
fn persist_video_cameras_writes_the_leg_list_and_preserves_the_rest() {
    // Dev-host end-to-end: load → set cameras → atomic-write → reload.
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(
        &cfg,
        "video:\n  wfb:\n    channel: 149\nagent:\n  profile: drone\n",
    )
    .unwrap();
    let lock = dir.path().join("config.yaml.lock");
    let cameras = serde_json::json!([
        {"id": "main", "source": "rtsp://192.168.144.25:8554/main", "role": "eo"},
        {"id": "ir", "source": "rtsp://192.168.144.25:8554/ir", "role": "ir", "codec": "h264"},
    ]);
    let outcome = persist_video_cameras(&cfg, &lock, &cameras, "operator");
    // On Linux as non-root this is Failed; on the dev host it writes.
    if cfg!(target_os = "linux") {
        return;
    }
    assert_eq!(outcome, CamerasPersist::Written);
    let reloaded = try_load_config_mapping(&cfg).unwrap();
    let video = reloaded.get("video").and_then(Value::as_mapping).unwrap();
    let legs = reloaded.get("cameras").and_then(Value::as_sequence).unwrap();
    assert_eq!(legs.len(), 2);
    let first = legs[0].as_mapping().unwrap();
    assert_eq!(first.get("id").unwrap(), &Value::String("main".into()));
    assert_eq!(
        first.get("source").unwrap(),
        &Value::String("rtsp://192.168.144.25:8554/main".into())
    );
    // Each written leg is stamped with the writer's owner.
    assert_eq!(
        first.get("owner").unwrap(),
        &Value::String("operator".into())
    );
    // Pre-existing video.wfb keys preserved (the set is a merge, not a replace).
    let wfb = video.get("wfb").and_then(Value::as_mapping).unwrap();
    assert_eq!(wfb.get("channel").unwrap(), &Value::Number(149.into()));
    assert!(reloaded.get("agent").is_some());
    // A1: the primary leg's source is mirrored into video.camera.source so
    // the inline pipeline serves it (a pod-only drone has no local camera).
    let camera = video.get("camera").and_then(Value::as_mapping).unwrap();
    assert_eq!(
        camera.get("source").unwrap(),
        &Value::String("rtsp://192.168.144.25:8554/main".into())
    );
}

#[test]
fn persist_video_cameras_refuses_to_clobber_an_unparseable_config() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    // A present but unparseable config (a corrupt / truncated write).
    std::fs::write(&cfg, "agent:\n  profile: drone\nvideo: [unclosed\n").unwrap();
    let lock = dir.path().join("config.yaml.lock");
    let before = std::fs::read_to_string(&cfg).unwrap();
    let cameras = serde_json::json!([{"id":"main","source":"rtsp://x/main","role":"eo"}]);
    let outcome = persist_video_cameras(&cfg, &lock, &cameras, "operator");
    assert_eq!(
        outcome,
        CamerasPersist::Failed,
        "must refuse to write over an unparseable config"
    );
    // The corrupt file is left untouched, not clobbered with an empty config
    // that would drop WFB pairing + profile.
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), before);
}

#[test]
fn apply_video_cameras_ignores_a_non_array() {
    // Defence-in-depth: a non-sequence value never mutates the config.
    let mut root = Mapping::new();
    apply_video_cameras(&mut root, &serde_json::json!({"id": "main"}), "operator").unwrap();
    assert!(root.get("video").is_none() && root.get("cameras").is_none());
    apply_video_cameras(&mut root, &serde_json::json!("main"), "operator").unwrap();
    assert!(root.get("video").is_none() && root.get("cameras").is_none());
}

fn cameras_after_apply(root: &Mapping) -> Vec<serde_json::Value> {
    root.get("cameras")
        .and_then(Value::as_sequence)
        .map(|s| {
            s.iter()
                .filter_map(|v| serde_json::to_value(v).ok())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn merge_by_owner_operator_write_preserves_plugin_legs() {
    // A plugin declared its pod legs; an operator write of a local cam must
    // keep the plugin legs (and vice versa).
    let mut root = Mapping::new();
    apply_video_cameras(
        &mut root,
        &serde_json::json!([
            {"id": "eo", "source": "rtsp://pod/main", "role": "primary"},
            {"id": "ir", "source": "rtsp://pod/ir", "role": "ir"},
        ]),
        "com.altnautica.siyi-pod",
    )
    .unwrap();
    apply_video_cameras(
        &mut root,
        &serde_json::json!([{"id": "belly", "source": "/dev/video2"}]),
        "operator",
    )
    .unwrap();
    let legs = cameras_after_apply(&root);
    let ids: Vec<&str> = legs
        .iter()
        .filter_map(|l| l.get("id").and_then(serde_json::Value::as_str))
        .collect();
    // Both plugin legs and the operator leg survive.
    assert!(ids.contains(&"eo"));
    assert!(ids.contains(&"ir"));
    assert!(ids.contains(&"belly"));
    assert_eq!(legs.len(), 3);
    // The operator leg carries the operator owner.
    let belly = legs.iter().find(|l| l["id"] == "belly").unwrap();
    assert_eq!(belly["owner"], "operator");
    let eo = legs.iter().find(|l| l["id"] == "eo").unwrap();
    assert_eq!(eo["owner"], "com.altnautica.siyi-pod");
}

#[test]
fn merge_by_owner_plugin_rewrite_drops_its_own_stale_leg() {
    // A plugin that previously declared [eo, ir, wide] and now declares
    // [eo, ir] must drop its own stale `wide` leg, while an operator leg is
    // untouched.
    let mut root = Mapping::new();
    apply_video_cameras(
        &mut root,
        &serde_json::json!([{"id": "belly", "source": "/dev/video2"}]),
        "operator",
    )
    .unwrap();
    apply_video_cameras(
        &mut root,
        &serde_json::json!([
            {"id": "eo", "source": "rtsp://pod/main", "role": "eo"},
            {"id": "ir", "source": "rtsp://pod/ir"},
            {"id": "wide", "source": "rtsp://pod/wide"},
        ]),
        "com.altnautica.siyi-pod",
    )
    .unwrap();
    // Re-declare with `wide` removed.
    apply_video_cameras(
        &mut root,
        &serde_json::json!([
            {"id": "eo", "source": "rtsp://pod/main", "role": "eo"},
            {"id": "ir", "source": "rtsp://pod/ir"},
        ]),
        "com.altnautica.siyi-pod",
    )
    .unwrap();
    let legs = cameras_after_apply(&root);
    let ids: Vec<&str> = legs
        .iter()
        .filter_map(|l| l.get("id").and_then(serde_json::Value::as_str))
        .collect();
    assert!(!ids.contains(&"wide"), "the plugin's stale leg is dropped");
    assert!(ids.contains(&"belly"), "the operator leg is preserved");
    assert!(ids.contains(&"eo"));
    assert!(ids.contains(&"ir"));
    assert_eq!(legs.len(), 3);
}

#[test]
fn only_the_operator_replaces_a_legacy_ownerless_leg_in_place() {
    // A leg written before the owner field existed belongs to the operator.
    // A plugin naming its id is refused with the config untouched; the
    // operator's re-declaration replaces it in place, with no duplicate.
    let mut root = Mapping::new();
    let seq = serde_json::json!([{"id": "eo", "source": "rtsp://old/eo"}]);
    root.insert(
        Value::String("cameras".into()),
        serde_norway::to_value(&seq).unwrap(),
    );
    let refused = apply_video_cameras(
        &mut root,
        &serde_json::json!([{"id": "eo", "source": "rtsp://pod/main"}]),
        "com.altnautica.siyi-pod",
    )
    .unwrap_err();
    assert!(refused.starts_with("owner_conflict"), "{refused}");
    assert_eq!(cameras_after_apply(&root)[0]["source"], "rtsp://old/eo");

    apply_video_cameras(
        &mut root,
        &serde_json::json!([{"id": "eo", "source": "/dev/video0", "role": "primary"}]),
        "operator",
    )
    .unwrap();
    let legs = cameras_after_apply(&root);
    assert_eq!(legs.len(), 1, "no duplicate id");
    assert_eq!(legs[0]["source"], "/dev/video0");
    assert_eq!(legs[0]["owner"], "operator");
    // The primary source was mirrored into video.camera.source.
    assert_eq!(camera_source(&root), Some("/dev/video0"));
}

fn camera_source(root: &Mapping) -> Option<&str> {
    root.get("video")
        .and_then(Value::as_mapping)
        .and_then(|v| v.get("camera"))
        .and_then(Value::as_mapping)
        .and_then(|c| c.get("source"))
        .and_then(Value::as_str)
}

#[test]
fn a_plugin_cannot_take_over_another_owners_leg_or_the_operator_primary() {
    let mut root = Mapping::new();
    apply_video_cameras(
        &mut root,
        &serde_json::json!([{"id": "belly", "source": "/dev/video2", "role": "primary"}]),
        "operator",
    )
    .unwrap();
    apply_video_cameras(
        &mut root,
        &serde_json::json!([{"id": "ir", "source": "rtsp://pod/ir"}]),
        "com.example.pod",
    )
    .unwrap();
    let before = cameras_after_apply(&root);

    // A plugin re-pointing the operator's leg id.
    let err = apply_video_cameras(
        &mut root,
        &serde_json::json!([{"id": "belly", "source": "rtsp://example.com/x"}]),
        "com.example.other",
    )
    .unwrap_err();
    assert!(err.contains("belongs to operator"), "{err}");
    // Another plugin's leg is just as protected.
    let err = apply_video_cameras(
        &mut root,
        &serde_json::json!([{"id": "ir", "source": "rtsp://example.com/x"}]),
        "com.example.other",
    )
    .unwrap_err();
    assert!(err.contains("belongs to com.example.pod"), "{err}");
    // A plugin declaring its own new leg as primary over the operator's.
    let err = apply_video_cameras(
        &mut root,
        &serde_json::json!([{"id": "eo", "source": "rtsp://pod/main", "role": "primary"}]),
        "com.example.pod",
    )
    .unwrap_err();
    assert!(err.starts_with("owner_conflict"), "{err}");
    // An operator write naming a plugin's leg id is refused too.
    assert!(apply_video_cameras(
        &mut root,
        &serde_json::json!([{"id": "ir", "source": "/dev/video4"}]),
        "operator",
    )
    .is_err());

    // Every refusal left the legs and the primary source as they were.
    assert_eq!(cameras_after_apply(&root), before);
    assert_eq!(camera_source(&root), Some("/dev/video2"));
}

#[test]
fn a_plugin_may_declare_the_primary_when_the_operator_has_none() {
    let mut root = Mapping::new();
    apply_video_cameras(
        &mut root,
        &serde_json::json!([{"id": "eo", "source": "rtsp://pod/main", "role": "primary"}]),
        "com.example.pod",
    )
    .unwrap();
    assert_eq!(camera_source(&root), Some("rtsp://pod/main"));
    // Re-declaring its own primary stays allowed.
    apply_video_cameras(
        &mut root,
        &serde_json::json!([{"id": "eo", "source": "rtsp://pod/zoom", "role": "primary"}]),
        "com.example.pod",
    )
    .unwrap();
    assert_eq!(camera_source(&root), Some("rtsp://pod/zoom"));
}
