use super::*;
use crate::wfb_pair_state::read_public_fingerprint;

/// Read a response body as JSON.
async fn body_json(resp: Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// A valid 64-byte key file whose fingerprint is computable; returns the
/// fingerprint the route would report.
fn write_key(dir: &Path, name: &str) -> String {
    let path = dir.join(name);
    let mut bytes = vec![0u8; 64];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = i as u8;
    }
    std::fs::write(&path, &bytes).unwrap();
    read_public_fingerprint(&path).unwrap()
}

// ── the disable path (enabled=false): always persists ─────────────────────

#[tokio::test]
async fn disable_on_an_unpaired_drone_persists_false_and_echoes_status() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    let keys = dir.path().join("wfb");
    let proof = dir.path().join("pair-proof.json");
    let retry = dir.path().join("auto-pair-retry.request");
    std::fs::create_dir_all(&keys).unwrap();
    // A drone with no key file, an existing auto-pair flag + an unrelated key.
    std::fs::write(
        &cfg,
        "agent:\n  name: my-drone\nvideo:\n  wfb:\n    channel: 149\n    auto_pair_enabled: true\n",
    )
    .unwrap();

    let resp = put_auto_pair_at(&cfg, &keys, &proof, &retry, "drone", false, false);
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body,
        json!({
            "paired": false,
            "paired_with_device_id": null,
            "paired_at": null,
            "fingerprint": null,
            "auto_pair_enabled": false,
            "role": "drone",
            "applied": true,
        })
    );

    // The flag landed in video.wfb; the unrelated channel + agent.name survived.
    let parsed: serde_norway::Value =
        serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    let wfb = parsed.get("video").and_then(|v| v.get("wfb")).unwrap();
    assert_eq!(
        wfb.get("auto_pair_enabled").and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(wfb.get("channel").and_then(|v| v.as_i64()), Some(149));
    assert_eq!(
        parsed
            .get("agent")
            .and_then(|a| a.get("name"))
            .and_then(|n| n.as_str()),
        Some("my-drone")
    );
}

#[tokio::test]
async fn enable_on_an_unpaired_drone_persists_true() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    let keys = dir.path().join("wfb");
    let proof = dir.path().join("pair-proof.json");
    let retry = dir.path().join("auto-pair-retry.request");
    std::fs::create_dir_all(&keys).unwrap();
    // Unpaired (no key) → enable is allowed and persists true.
    std::fs::write(&cfg, "agent:\n  profile: drone\n").unwrap();

    let resp = put_auto_pair_at(&cfg, &keys, &proof, &retry, "drone", true, false);
    let body = body_json(resp).await;
    assert_eq!(body["auto_pair_enabled"], json!(true));
    assert!(body.get("rearm_blocked").is_none());
    // Arming an unpaired rig asks the supervisor to retry the local bind:
    // its failover signal returns to local and its attempt count restarts.
    assert_eq!(body["retry_requested"], json!(true));
    assert_eq!(body["applied"], json!(true));
    assert!(retry.exists());
    let parsed: serde_norway::Value =
        serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    assert_eq!(
        parsed
            .get("video")
            .and_then(|v| v.get("wfb"))
            .and_then(|w| w.get("auto_pair_enabled"))
            .and_then(|v| v.as_bool()),
        Some(true)
    );
}

// ── the re-arm-blocked path (enabled=true on a paired rig): no persist ────

#[tokio::test]
async fn rearm_on_a_paired_drone_is_blocked_and_does_not_persist() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    let keys = dir.path().join("wfb");
    let proof = dir.path().join("pair-proof.json");
    let retry = dir.path().join("auto-pair-retry.request");
    std::fs::create_dir_all(&keys).unwrap();
    let fp = write_key(&keys, "tx.key");
    // A paired drone (tx.key present) with a peer + a disarmed flag on disk.
    std::fs::write(
            &cfg,
            "agent:\n  profile: drone\nvideo:\n  wfb:\n    paired_with_device_id: peer-xyz\n    auto_pair_enabled: false\n",
        )
        .unwrap();
    let before = std::fs::read_to_string(&cfg).unwrap();

    let resp = put_auto_pair_at(&cfg, &keys, &proof, &retry, "drone", true, false);
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body,
        json!({
            "paired": true,
            "paired_with_device_id": "peer-xyz",
            "paired_at": null,
            "fingerprint": fp,
            "auto_pair_enabled": false,
            "rearm_blocked": true,
            "role": "drone",
            "applied": false,
        })
    );
    // The file is unchanged — the refuse path persists nothing.
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), before);
    // And nothing was recorded against the key.
    assert!(!proof.exists());
    assert!(!retry.exists(), "a refused re-arm must not request a retry");
}

#[tokio::test]
async fn an_unparseable_config_is_a_500_and_is_left_untouched() {
    // A duplicate key a hand edit left behind: the write must refuse rather
    // than replace the operator's whole document with three pair fields, and
    // the route must not claim success.
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    let keys = dir.path().join("wfb");
    let proof = dir.path().join("pair-proof.json");
    let retry = dir.path().join("auto-pair-retry.request");
    std::fs::create_dir_all(&keys).unwrap();
    let original = "agent:\n  profile: drone\nvideo:\n  a: 1\nvideo:\n  b: 2\n";
    std::fs::write(&cfg, original).unwrap();

    let resp = put_auto_pair_at(&cfg, &keys, &proof, &retry, "drone", true, false);
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = body_json(resp).await;
    assert_eq!(body["detail"]["error"], json!("config_write_failed"));
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original);
    assert!(!retry.exists());
}

#[tokio::test]
async fn a_retry_request_the_supervisor_cannot_see_is_a_500() {
    // The request path's parent is a file, so the request cannot be written.
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    let keys = dir.path().join("wfb");
    let proof = dir.path().join("pair-proof.json");
    std::fs::create_dir_all(&keys).unwrap();
    std::fs::write(dir.path().join("blocker"), "x").unwrap();
    let retry = dir.path().join("blocker").join("auto-pair-retry.request");
    std::fs::write(&cfg, "agent:\n  profile: drone\n").unwrap();

    let resp = put_auto_pair_at(&cfg, &keys, &proof, &retry, "drone", true, false);
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        body_json(resp).await["detail"]["error"],
        json!("retry_request_failed")
    );
}

// ── the forced re-arm: granted, one-shot, and the key is never touched ────

#[tokio::test]
async fn a_forced_rearm_on_a_paired_drone_is_granted_and_leaves_the_key_alone() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    let keys = dir.path().join("wfb");
    let proof = dir.path().join("pair-proof.json");
    let retry = dir.path().join("auto-pair-retry.request");
    std::fs::create_dir_all(&keys).unwrap();
    let fp = write_key(&keys, "tx.key");
    let key_before = std::fs::read(keys.join("tx.key")).unwrap();
    std::fs::write(
            &cfg,
            "agent:\n  profile: drone\nvideo:\n  wfb:\n    paired_with_device_id: peer-xyz\n    auto_pair_enabled: false\n",
        )
        .unwrap();

    let resp = put_auto_pair_at(&cfg, &keys, &proof, &retry, "drone", true, true);
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["rearm_blocked"], json!(false));
    assert_eq!(body["forced"], json!(true));
    assert_eq!(body["auto_pair_enabled"], json!(true));
    assert_eq!(body["fingerprint"], json!(fp));

    // The one-shot is recorded against the key's OWN fingerprint, so a key
    // replaced before the supervisor's next tick discards it rather than
    // firing at whatever key happens to be there.
    let stored = ados_protocol::pair_proof::read_pair_proof_from(&proof).unwrap();
    assert!(stored.force_rearm);
    assert_eq!(stored.key_fingerprint, fp);
    assert_eq!(stored.role, "drone");

    // The whole point: the key is still there. Unpairing first would have
    // deleted it, which is the worst move if the key turns out to be fine.
    assert_eq!(std::fs::read(keys.join("tx.key")).unwrap(), key_before);

    // And the arm flag is persisted, so the supervisor sees a live request.
    let parsed: serde_norway::Value =
        serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    assert_eq!(
        parsed
            .get("video")
            .and_then(|v| v.get("wfb"))
            .and_then(|w| w.get("auto_pair_enabled"))
            .and_then(|v| v.as_bool()),
        Some(true)
    );
}

#[tokio::test]
async fn a_forced_rearm_preserves_what_is_already_known_about_the_key() {
    // Forcing one window is not a reason to forget the key's proof or the
    // episodes already spent on it.
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    let keys = dir.path().join("wfb");
    let proof = dir.path().join("pair-proof.json");
    let retry = dir.path().join("auto-pair-retry.request");
    std::fs::create_dir_all(&keys).unwrap();
    let fp = write_key(&keys, "tx.key");
    std::fs::write(&cfg, "agent:\n  profile: drone\n").unwrap();

    let mut existing = ados_protocol::pair_proof::PairProof::fresh("drone", &fp);
    existing.mark_proven(1_700_000_000);
    existing.record_rearm(1_700_000_100);
    ados_protocol::pair_proof::write_pair_proof_to(&proof, &existing).unwrap();

    let _ = put_auto_pair_at(&cfg, &keys, &proof, &retry, "drone", true, true);
    let stored = ados_protocol::pair_proof::read_pair_proof_from(&proof).unwrap();
    assert!(stored.force_rearm);
    assert_eq!(stored.proven_at, existing.proven_at);
    assert_eq!(stored.rearm_episodes, existing.rearm_episodes);
}

#[tokio::test]
async fn force_on_an_unpaired_rig_is_an_ordinary_arm() {
    // There is nothing to override and no key to key a one-shot to, so the
    // request must not manufacture a record.
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    let keys = dir.path().join("wfb");
    let proof = dir.path().join("pair-proof.json");
    let retry = dir.path().join("auto-pair-retry.request");
    std::fs::create_dir_all(&keys).unwrap();
    std::fs::write(&cfg, "agent:\n  profile: drone\n").unwrap();

    let body = body_json(put_auto_pair_at(
        &cfg, &keys, &proof, &retry, "drone", true, true,
    ))
    .await;
    assert_eq!(body["auto_pair_enabled"], json!(true));
    assert!(body.get("forced").is_none());
    assert!(body.get("rearm_blocked").is_none());
    assert!(!proof.exists());
    assert!(retry.exists());
}

#[tokio::test]
async fn force_is_ignored_when_disabling() {
    // `enabled: false` is a disarm; force has nothing to grant.
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    let keys = dir.path().join("wfb");
    let proof = dir.path().join("pair-proof.json");
    let retry = dir.path().join("auto-pair-retry.request");
    std::fs::create_dir_all(&keys).unwrap();
    write_key(&keys, "tx.key");
    std::fs::write(&cfg, "agent:\n  profile: drone\n").unwrap();

    let body = body_json(put_auto_pair_at(
        &cfg, &keys, &proof, &retry, "drone", false, true,
    ))
    .await;
    assert_eq!(body["auto_pair_enabled"], json!(false));
    assert!(body.get("forced").is_none());
    assert!(!proof.exists(), "a disarm must not record a one-shot");
    assert!(!retry.exists(), "a disarm must not request a retry");
}

#[test]
fn the_force_field_defaults_off_so_an_existing_client_is_unchanged() {
    // The GCS pairing card sends `{"enabled": ...}` and must keep getting the
    // refusal, not a silent re-bind of a working pair.
    let req: AutoPairToggleRequest = serde_json::from_str(r#"{"enabled": true}"#).unwrap();
    assert!(req.enabled);
    assert!(!req.force);
    let req: AutoPairToggleRequest =
        serde_json::from_str(r#"{"enabled": true, "force": true}"#).unwrap();
    assert!(req.force);
}

#[tokio::test]
async fn disable_on_a_paired_drone_is_allowed_and_persists() {
    // enabled=false is never blocked, even when paired.
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    let keys = dir.path().join("wfb");
    let proof = dir.path().join("pair-proof.json");
    let retry = dir.path().join("auto-pair-retry.request");
    std::fs::create_dir_all(&keys).unwrap();
    write_key(&keys, "tx.key");
    std::fs::write(
            &cfg,
            "agent:\n  profile: drone\nvideo:\n  wfb:\n    paired_with_device_id: peer-xyz\n    auto_pair_enabled: true\n",
        )
        .unwrap();

    let resp = put_auto_pair_at(&cfg, &keys, &proof, &retry, "drone", false, false);
    let body = body_json(resp).await;
    assert_eq!(body["auto_pair_enabled"], json!(false));
    assert!(body.get("rearm_blocked").is_none());
    // The peer survives the persist (status read it, persist wrote it back).
    let parsed: serde_norway::Value =
        serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    let wfb = parsed.get("video").and_then(|v| v.get("wfb")).unwrap();
    assert_eq!(
        wfb.get("auto_pair_enabled").and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        wfb.get("paired_with_device_id").and_then(|v| v.as_str()),
        Some("peer-xyz")
    );
}

// ── the toggle writes the arm flag and nothing else ───────────────────────

/// The bind stores `paired_at` as an unquoted ISO timestamp, which the
/// status projection reads as null. The toggle must leave the record the
/// bind wrote exactly as it was, on both profiles, rather than write that
/// lossy projection back over it.
#[tokio::test]
async fn the_toggle_leaves_the_bind_record_untouched() {
    for (profile, role) in [("drone", "drone"), ("ground_station", "gs")] {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let keys = dir.path().join("wfb");
        let proof = dir.path().join("pair-proof.json");
        let retry = dir.path().join("auto-pair-retry.request");
        std::fs::create_dir_all(&keys).unwrap();
        std::fs::write(
                &cfg,
                format!(
                    "agent:\n  profile: {profile}\nvideo:\n  wfb:\n    paired_with_device_id: peer-1\n    paired_at: 2026-05-29T12:34:56+00:00\n    auto_pair_enabled: true\n"
                ),
            )
            .unwrap();
        let resp = put_auto_pair_at(&cfg, &keys, &proof, &retry, role, false, false);
        assert_eq!(resp.status(), StatusCode::OK);
        let parsed: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let wfb = parsed.get("video").and_then(|v| v.get("wfb")).unwrap();
        assert_eq!(
            wfb.get("paired_at").and_then(|v| v.as_str()),
            Some("2026-05-29T12:34:56+00:00"),
            "{role}: paired_at survives the toggle"
        );
        assert_eq!(
            wfb.get("paired_with_device_id").and_then(|v| v.as_str()),
            Some("peer-1")
        );
        assert_eq!(
            wfb.get("auto_pair_enabled").and_then(|v| v.as_bool()),
            Some(false)
        );
        assert!(
            parsed.get("ground_station").is_none(),
            "{role}: no legacy mirror is written by the toggle"
        );
    }
}

/// A forced re-arm whose one-shot cannot be recorded must not leave the rig
/// armed: the caller is told the change did not land, so nothing may have.
#[tokio::test]
async fn a_forced_rearm_that_cannot_record_its_one_shot_leaves_the_flag_alone() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    let keys = dir.path().join("wfb");
    std::fs::write(dir.path().join("blocker"), b"x").unwrap();
    let proof = dir.path().join("blocker").join("pair-proof.json");
    let retry = dir.path().join("auto-pair-retry.request");
    std::fs::create_dir_all(&keys).unwrap();
    write_key(&keys, "tx.key");
    let original = "agent:\n  profile: drone\nvideo:\n  wfb:\n    paired_with_device_id: peer-xyz\n    auto_pair_enabled: false\n";
    std::fs::write(&cfg, original).unwrap();

    let resp = put_auto_pair_at(&cfg, &keys, &proof, &retry, "drone", true, true);
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        body_json(resp).await["detail"]["error"],
        json!("rearm_record_failed")
    );
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original);
}

/// A forced re-arm with the latch switched off is refused and records
/// nothing, so no one-shot is left to fire when the latch is re-enabled.
#[tokio::test]
async fn a_forced_rearm_with_the_latch_off_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    let keys = dir.path().join("wfb");
    let proof = dir.path().join("pair-proof.json");
    let retry = dir.path().join("auto-pair-retry.request");
    std::fs::create_dir_all(&keys).unwrap();
    write_key(&keys, "tx.key");
    let original = "agent:\n  profile: drone\nvideo:\n  wfb:\n    auto_pair_enabled: false\n    pair_rearm:\n      enabled: false\n";
    std::fs::write(&cfg, original).unwrap();
    let resp = put_auto_pair_at(&cfg, &keys, &proof, &retry, "drone", true, true);
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(
        body_json(resp).await["detail"]["error"],
        json!("rearm_latch_disabled")
    );
    assert!(!proof.exists(), "no one-shot recorded");
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original);
}

#[tokio::test]
async fn gs_toggle_persists_the_flag() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    let keys = dir.path().join("wfb");
    let proof = dir.path().join("pair-proof.json");
    let retry = dir.path().join("auto-pair-retry.request");
    std::fs::create_dir_all(&keys).unwrap();
    // A GS with a peer recorded under the canonical spot, unpaired (no rx.key),
    // so the enable persist runs (not blocked).
    std::fs::write(
            &cfg,
            "agent:\n  profile: ground_station\nvideo:\n  wfb:\n    paired_with_device_id: drone-1\n    auto_pair_enabled: false\n",
        )
        .unwrap();

    let resp = put_auto_pair_at(&cfg, &keys, &proof, &retry, "gs", true, false);
    let body = body_json(resp).await;
    assert_eq!(body["auto_pair_enabled"], json!(true));
    assert_eq!(body["role"], json!("gs"));
    assert_eq!(body["paired_with_device_id"], json!("drone-1"));

    let parsed: serde_norway::Value =
        serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    assert!(parsed.get("ground_station").is_none());
    assert_eq!(
        parsed
            .get("video")
            .and_then(|v| v.get("wfb"))
            .and_then(|w| w.get("auto_pair_enabled"))
            .and_then(|v| v.as_bool()),
        Some(true)
    );
}

#[tokio::test]
async fn disable_with_no_peer_writes_only_the_flag() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    let keys = dir.path().join("wfb");
    let proof = dir.path().join("pair-proof.json");
    let retry = dir.path().join("auto-pair-retry.request");
    std::fs::create_dir_all(&keys).unwrap();
    std::fs::write(&cfg, "agent:\n  profile: drone\n").unwrap();
    let resp = put_auto_pair_at(&cfg, &keys, &proof, &retry, "drone", false, false);
    let body = body_json(resp).await;
    assert_eq!(body["paired_with_device_id"], Value::Null);
    let parsed: serde_norway::Value =
        serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    let wfb = parsed.get("video").and_then(|v| v.get("wfb")).unwrap();
    assert!(wfb.get("paired_with_device_id").is_none());
    assert!(wfb.get("paired_at").is_none());
    assert_eq!(
        wfb.get("auto_pair_enabled").and_then(|v| v.as_bool()),
        Some(false)
    );
}
