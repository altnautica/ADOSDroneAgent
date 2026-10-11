use super::*;
// The channel table is the shared one now; this route's guard is still
// what these pin, so the import comes in rather than the table coming back.
use ados_protocol::wfb_status::get_channel;

// ── channel lookup + the invalid-channel guard ────────────────────────────

#[test]
fn get_channel_resolves_the_standard_set() {
    assert_eq!(get_channel(149).unwrap().frequency_mhz, 5745);
    assert_eq!(get_channel(36).unwrap().frequency_mhz, 5180);
    assert_eq!(get_channel(165).unwrap().frequency_mhz, 5825);
    // A non-standard channel is unknown.
    assert!(get_channel(7).is_none());
    assert!(get_channel(100).is_none());
}

#[test]
fn valid_channels_repr_matches_the_python_list() {
    // The exact `str([c.channel_number for c in STANDARD_CHANNELS])` text.
    assert_eq!(
        valid_channels_repr(),
        "[36, 40, 44, 48, 149, 153, 157, 161, 165]"
    );
}

#[tokio::test]
async fn an_invalid_channel_is_a_400_with_the_valid_list() {
    // The channel guard fires before any socket touch, so an absent socket
    // path under the tempdir is irrelevant.
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("radio-cmd.sock");
    let resp = set_wfb_channel_at(&sock, 7).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_detail_string(resp).await,
        "Invalid channel 7. Valid channels: [36, 40, 44, 48, 149, 153, 157, 161, 165]"
    );
}

#[tokio::test]
async fn a_valid_channel_with_no_radio_socket_is_a_503() {
    // An absent radio-cmd.sock → the roundtrip is None → the no-manager 503,
    // the FastAPI `wfb is None` terminal posture.
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("radio-cmd.sock");
    let resp = set_wfb_channel_at(&sock, 149).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body_detail_string(resp).await, "WFB-ng service not running");
}

// ── channel reply mapping: ok / refused ───────────────────────────────────

#[tokio::test]
async fn an_ok_reply_is_an_initiated_hop_not_a_committed_channel() {
    // The radio answers `ok` once it has validated the hop and BEFORE the
    // announce: the ground station may never ack and the link stay put, so
    // the route must not report the channel as changed.
    let reply = json!({"ok": true, "channel": 153});
    let resp = channel_reply_response(&reply, 153, 5765);
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    assert_eq!(
        body,
        json!({"status": "initiated", "target_channel": 153, "frequency_mhz": 5765})
    );
}

#[tokio::test]
async fn an_ok_reply_without_an_echoed_channel_targets_the_request() {
    let reply = json!({"ok": true});
    let resp = channel_reply_response(&reply, 149, 5745);
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = body_json(resp).await;
    assert_eq!(
        body,
        json!({"status": "initiated", "target_channel": 149, "frequency_mhz": 5745})
    );
}

#[test]
fn radio_socket_failures_map_to_503_504_and_502() {
    assert_eq!(
        radio_failure_response(CmdFailure::Unreachable).status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        radio_failure_response(CmdFailure::Timeout).status(),
        StatusCode::GATEWAY_TIMEOUT
    );
    assert_eq!(
        radio_failure_response(CmdFailure::BadReply).status(),
        StatusCode::BAD_GATEWAY
    );
}

#[tokio::test]
async fn a_refused_reply_is_a_409_with_the_error_object() {
    let reply = json!({"ok": false, "error": "peer did not ack"});
    let resp = channel_reply_response(&reply, 149, 5745);
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = body_json(resp).await;
    assert_eq!(
        body,
        json!({"detail": {"error": "hop_refused", "message": "peer did not ack"}})
    );
}

#[tokio::test]
async fn a_refused_reply_without_an_error_uses_the_default_message() {
    // ok absent / not true and no error text → the default "hop refused".
    let reply = json!({"ok": false});
    let resp = channel_reply_response(&reply, 149, 5745);
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = body_json(resp).await;
    assert_eq!(
        body,
        json!({"detail": {"error": "hop_refused", "message": "hop refused"}})
    );
}

/// A full POST against a live radio command socket that acks the hop: the route
/// forwards `{"op":"hop","channel":N}` and returns the success body with the
/// echoed channel.
#[tokio::test]
async fn full_channel_post_against_a_live_socket_acks() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("radio-cmd.sock");
    let listener = UnixListener::bind(&sock).unwrap();

    // The server reads the one-line request and replies with an ok hop.
    let server = tokio::spawn(async move {
        let (mut conn, _addr) = listener.accept().await.unwrap();
        let mut buf = [0u8; 1024];
        let n = conn.read(&mut buf).await.unwrap();
        let req: Value =
            serde_json::from_str(std::str::from_utf8(&buf[..n]).unwrap().trim()).unwrap();
        conn.write_all(b"{\"ok\": true, \"channel\": 149}\n")
            .await
            .unwrap();
        req
    });

    let resp = set_wfb_channel_at(&sock, 149).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let req = server.await.unwrap();
    assert_eq!(req, json!({"op": "hop", "channel": 149}));
    let body = body_json(resp).await;
    assert_eq!(
        body,
        json!({"status": "initiated", "target_channel": 149, "frequency_mhz": 5745})
    );
}

// ── tx-power: the floor / ceiling guards ──────────────────────────────────

#[tokio::test]
async fn tx_power_below_the_floor_is_a_400_below_floor() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("wfb-cmd.sock");
    let cfg = dir.path().join("config.yaml");
    let resp = set_wfb_tx_power_at(&sock, &cfg, 0).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(body, json!({"detail": {"error": "below_floor", "min": 1}}));
}

#[tokio::test]
async fn tx_power_above_the_default_ceiling_is_a_400_above_ceiling() {
    // With no config the ceiling defaults to 15; 20 is above it.
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("wfb-cmd.sock");
    let cfg = dir.path().join("config.yaml");
    let resp = set_wfb_tx_power_at(&sock, &cfg, 20).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert_eq!(
        body,
        json!({"detail": {"error": "above_ceiling", "max": 15}})
    );
}

#[test]
fn configured_ceiling_reads_the_config_field() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(&cfg, "video:\n  wfb:\n    tx_power_max_dbm: 22\n").unwrap();
    assert_eq!(configured_tx_power_ceiling(&cfg), 22);
}

#[test]
fn configured_ceiling_defaults_to_fifteen_when_absent() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(&cfg, "video:\n  wfb:\n    channel: 149\n").unwrap();
    assert_eq!(configured_tx_power_ceiling(&cfg), 15);
    // An absent file also reads the default.
    assert_eq!(
        configured_tx_power_ceiling(&dir.path().join("absent.yaml")),
        15
    );
}

// ── tx-power: the effective-dbm reply mapping ─────────────────────────────

#[test]
fn effective_reads_a_numeric_reply() {
    let reply = json!({"ok": true, "effective_dbm": 10});
    assert_eq!(tx_power_effective_from_reply(&reply), Ok(json!(10)));
    // A float effective is coerced to an integer (Python `int(eff)`).
    let reply = json!({"ok": true, "effective_dbm": 12.0});
    assert_eq!(tx_power_effective_from_reply(&reply), Ok(json!(12)));
}

#[test]
fn effective_is_null_when_every_ramp_step_was_rejected() {
    // The driver rejected every step → effective_dbm absent/null → the success
    // body reports null (Python `None`).
    let reply = json!({"ok": true, "effective_dbm": Value::Null});
    assert_eq!(tx_power_effective_from_reply(&reply), Ok(Value::Null));
    let reply = json!({"ok": true});
    assert_eq!(tx_power_effective_from_reply(&reply), Ok(Value::Null));
}

#[test]
fn an_apply_failure_reply_is_an_error_with_the_radio_message() {
    let reply = json!({"ok": false, "error": "txpower set failed"});
    assert_eq!(
        tx_power_effective_from_reply(&reply),
        Err(TxPowerRefusal::Refused("txpower set failed".to_string()))
    );
    // ok: false with no error text uses the Python default message.
    let reply = json!({"ok": false});
    assert_eq!(
        tx_power_effective_from_reply(&reply),
        Err(TxPowerRefusal::Refused(
            "unknown radio command error".to_string()
        ))
    );
}

#[tokio::test]
async fn a_reply_that_confirms_nothing_is_a_502_and_is_not_persisted() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for reply in [json!({"error": "busy"}), json!({"ok": "yes"}), json!({})] {
        assert_eq!(
            tx_power_effective_from_reply(&reply),
            Err(TxPowerRefusal::Unreadable),
            "{reply}"
        );
    }
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("wfb-cmd.sock");
    let cfg = dir.path().join("config.yaml");
    let listener = tokio::net::UnixListener::bind(&sock).unwrap();
    let server = tokio::spawn(async move {
        let (mut conn, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 256];
        let _ = conn.read(&mut buf).await;
        let _ = conn.write_all(b"{\"error\":\"busy\"}\n").await;
    });
    let resp = set_wfb_tx_power_at(&sock, &cfg, 10).await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    assert!(!cfg.exists(), "an unconfirmed apply must not be persisted");
    server.await.unwrap();
}

#[tokio::test]
async fn tx_power_with_no_radio_socket_is_a_503() {
    // An accepted value (within the floor/ceiling) but an unreachable command
    // socket is the no-manager 503; no persist happens on that path.
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("wfb-cmd.sock");
    let cfg = dir.path().join("config.yaml");
    let resp = set_wfb_tx_power_at(&sock, &cfg, 10).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body_detail_string(resp).await, "WFB-ng service not running");
    // No persist on the 503 path: the config file was never created.
    assert!(!cfg.exists());
}

/// A full PUT against a live wfb command socket that accepts the value: the
/// route forwards `{"op":"set_tx_power","tx_power_dbm":N}`, persists the value
/// to config, and returns the success body with the effective dBm + ceiling.
#[tokio::test]
async fn full_tx_power_put_against_a_live_socket_acks_and_persists() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    // Seed the config with a non-default ceiling + an unrelated key to prove the
    // persist preserves the rest of the file.
    std::fs::write(
        &cfg,
        "agent:\n  name: my-drone\nvideo:\n  wfb:\n    channel: 149\n    tx_power_max_dbm: 18\n",
    )
    .unwrap();

    let sock = dir.path().join("wfb-cmd.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let server = tokio::spawn(async move {
        let (mut conn, _addr) = listener.accept().await.unwrap();
        let mut buf = [0u8; 1024];
        let n = conn.read(&mut buf).await.unwrap();
        let req: Value =
            serde_json::from_str(std::str::from_utf8(&buf[..n]).unwrap().trim()).unwrap();
        conn.write_all(b"{\"ok\": true, \"effective_dbm\": 10}\n")
            .await
            .unwrap();
        req
    });

    let resp = set_wfb_tx_power_at(&sock, &cfg, 10).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let req = server.await.unwrap();
    assert_eq!(req, json!({"op": "set_tx_power", "tx_power_dbm": 10}));
    let body = body_json(resp).await;
    assert_eq!(
        body,
        json!({"requested_dbm": 10, "effective_dbm": 10, "tx_power_max_dbm": 18, "persisted": true})
    );

    // The persist wrote tx_power_dbm into video.wfb and kept the rest.
    let written = std::fs::read_to_string(&cfg).unwrap();
    let parsed: serde_norway::Value = serde_norway::from_str(&written).unwrap();
    let wfb = parsed.get("video").and_then(|v| v.get("wfb")).unwrap();
    assert_eq!(wfb.get("tx_power_dbm").and_then(norway_to_i64), Some(10));
    assert_eq!(wfb.get("channel").and_then(norway_to_i64), Some(149));
    assert_eq!(
        wfb.get("tx_power_max_dbm").and_then(norway_to_i64),
        Some(18)
    );
    // The unrelated agent.name survived.
    assert_eq!(
        parsed
            .get("agent")
            .and_then(|a| a.get("name"))
            .and_then(|n| n.as_str()),
        Some("my-drone")
    );
}

#[tokio::test]
async fn an_unparseable_config_is_not_overwritten_and_the_body_says_so() {
    // A hand edit left a duplicate `video:` key, which this parser refuses.
    // The radio still takes the value, but the operator's config must come
    // through byte-for-byte and the body must say the value will not persist.
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("wfb-cmd.sock");
    let cfg = dir.path().join("config.yaml");
    let original = "agent:\n  name: my-drone\nvideo:\n  wfb:\n    tx_power_max_dbm: 18\nvideo:\n  wfb:\n    channel: 149\n";
    std::fs::write(&cfg, original).unwrap();

    let listener = tokio::net::UnixListener::bind(&sock).unwrap();
    let server = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut conn, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 256];
        let _ = conn.read(&mut buf).await.unwrap();
        conn.write_all(b"{\"ok\": true, \"effective_dbm\": 10}\n")
            .await
            .unwrap();
    });

    let resp = set_wfb_tx_power_at(&sock, &cfg, 10).await;
    server.await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["persisted"], json!(false));
    assert!(body["persist_error"]
        .as_str()
        .unwrap()
        .contains("unparseable"));
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original);
}

// ── persist: preserves the rest of the file + creates missing sections ─────

#[test]
fn persist_creates_the_video_wfb_section_when_absent() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    std::fs::write(&cfg, "agent:\n  name: my-drone\n").unwrap();
    persist_tx_power(&cfg, 7).expect("persist should succeed");

    let parsed: serde_norway::Value =
        serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    let wfb = parsed.get("video").and_then(|v| v.get("wfb")).unwrap();
    assert_eq!(wfb.get("tx_power_dbm").and_then(norway_to_i64), Some(7));
    // The pre-existing agent section is untouched.
    assert_eq!(
        parsed
            .get("agent")
            .and_then(|a| a.get("name"))
            .and_then(|n| n.as_str()),
        Some("my-drone")
    );
}

#[test]
fn persist_creates_the_file_when_absent() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.yaml");
    persist_tx_power(&cfg, 9).expect("persist should succeed");
    let parsed: serde_norway::Value =
        serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
    assert_eq!(
        parsed
            .get("video")
            .and_then(|v| v.get("wfb"))
            .and_then(|w| w.get("tx_power_dbm"))
            .and_then(norway_to_i64),
        Some(9)
    );
}

#[test]
fn persist_failure_names_the_path_and_the_cause() {
    // An unwritable config directory is the realistic version of this: the
    // radio accepts the new power, the save fails, and the setting reverts
    // on the next restart. Previously every one of these collapsed to a bare
    // `false` at the call site, so nothing anywhere said which step failed
    // or why. The message is what makes that diagnosable.
    let dir = tempfile::tempdir().unwrap();
    let readonly = dir.path().join("locked");
    std::fs::create_dir(&readonly).unwrap();
    let cfg = readonly.join("config.yaml");
    std::fs::write(&cfg, "agent:\n  name: my-drone\n").unwrap();

    let mut perms = std::fs::metadata(&readonly).unwrap().permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o500); // r-x: the temp sibling cannot be created
    }
    std::fs::set_permissions(&readonly, perms.clone()).unwrap();

    // Confirm the mode actually denies a write for THIS user before relying
    // on it. Root ignores the mode, so without this the assertion below
    // would fail for a reason unrelated to the code under test.
    let enforced = std::fs::write(readonly.join(".probe"), b"x").is_err();

    let err = persist_tx_power(&cfg, 11);

    // Restore before asserting so a failed assert cannot leave an
    // undeletable temp dir behind.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut restore = perms;
        restore.set_mode(0o700);
        std::fs::set_permissions(&readonly, restore).unwrap();
    }

    if !enforced {
        return; // running as a user the mode does not bind (e.g. root)
    }

    let msg = err
        .expect_err("a read-only config dir must not report success")
        .to_string();
    assert!(
        msg.contains("config.yaml"),
        "the message must name the path it failed to write, got: {msg}"
    );
}

// ── shared body helpers ───────────────────────────────────────────────────

/// Read the `{"detail": "<string>"}` body off a response.
async fn body_detail_string(resp: Response) -> String {
    body_json(resp).await["detail"]
        .as_str()
        .unwrap()
        .to_string()
}

/// Read a response body as JSON.
async fn body_json(resp: Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}
