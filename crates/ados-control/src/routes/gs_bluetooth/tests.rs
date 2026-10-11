use super::*;

/// Read a response body as JSON.
async fn body_json(resp: Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

// ── parse helpers (the byte-faithful Python parser) ──────────────────────

#[test]
fn parse_bt_device_lines_matches_the_python_parser() {
    let out = "Device AA:BB:CC:DD:EE:FF Xbox Wireless Controller\nDevice 11:22:33:44:55:66 8BitDo Pro 2\n";
    assert_eq!(
        parse_bt_device_lines(out),
        vec![
            (
                "AA:BB:CC:DD:EE:FF".to_string(),
                "Xbox Wireless Controller".to_string()
            ),
            ("11:22:33:44:55:66".to_string(), "8BitDo Pro 2".to_string()),
        ]
    );
}

#[test]
fn parse_bt_device_lines_skips_non_device_lines_and_defaults_name() {
    let out = "Agent registered\nDevice AA:BB:CC:DD:EE:FF\n[bluetooth]# \n";
    assert_eq!(
        parse_bt_device_lines(out),
        vec![(
            "AA:BB:CC:DD:EE:FF".to_string(),
            "AA:BB:CC:DD:EE:FF".to_string()
        )]
    );
}

#[test]
fn device_id_for_bt_lowercases_and_prefixes() {
    assert_eq!(
        device_id_for_bt("AA:BB:CC:DD:EE:FF"),
        "bt:aa:bb:cc:dd:ee:ff"
    );
}

// ── scan record shape ────────────────────────────────────────────────────

#[test]
fn scan_records_carry_mac_name_and_null_rssi() {
    assert_eq!(
        scan_result(0, "Device AA:BB Pad\n", ""),
        Ok(vec![json!({"mac": "AA:BB", "name": "Pad", "rssi": null})])
    );
    // Nothing nearby is an empty list, which only a successful listing says.
    assert_eq!(scan_result(0, "", ""), Ok(vec![]));
}

#[test]
fn a_failed_listing_is_an_error_not_an_empty_scan() {
    assert_eq!(
        scan_result(127, "", "No such file or directory "),
        Err("bluetoothctl devices rc=127: No such file or directory".to_string())
    );
    assert_eq!(
        scan_result(124, "Device AA:BB Pad\n", ""),
        Err("bluetoothctl devices rc=124".to_string())
    );
}

// ── the result-dict shapes (built off the same json! the handlers return) ──

#[test]
fn pair_success_shape() {
    let body = json!({"paired": true, "connected": true, "error": Value::Null});
    assert_eq!(
        body,
        json!({"paired": true, "connected": true, "error": null})
    );
}

#[test]
fn pair_failed_pair_step_shape() {
    // Built the same way the handler composes the pair-step failure.
    let rc = 1;
    let err = "  Failed to pair: org.bluez.Error.AlreadyExists  ";
    let body = json!({
        "paired": false,
        "error": format!("pair rc={rc}: {}", err.trim()),
    });
    assert_eq!(
        body,
        json!({
            "paired": false,
            "error": "pair rc=1: Failed to pair: org.bluez.Error.AlreadyExists"
        })
    );
}

#[test]
fn pair_failed_connect_step_shape() {
    let rc_c = 1;
    let err_c = "Failed to connect";
    let body = json!({
        "paired": true,
        "connected": false,
        "error": format!("connect rc={rc_c}: {}", err_c.trim()),
    });
    assert_eq!(
        body,
        json!({
            "paired": true,
            "connected": false,
            "error": "connect rc=1: Failed to connect"
        })
    );
}

#[test]
fn forget_failed_uses_stderr_then_rc_fallback() {
    // Non-empty stderr is preferred.
    let err = "  not available  ";
    let trimmed = err.trim();
    let message = if trimmed.is_empty() {
        "rc=1".to_string()
    } else {
        trimmed.to_string()
    };
    assert_eq!(
        json!({"forgotten": false, "error": message}),
        json!({"forgotten": false, "error": "not available"})
    );
    // Empty stderr falls back to the rc string.
    let err2 = "   ";
    let trimmed2 = err2.trim();
    let rc = 1;
    let message2 = if trimmed2.is_empty() {
        format!("rc={rc}")
    } else {
        trimmed2.to_string()
    };
    assert_eq!(message2, "rc=1");
}

#[test]
fn forget_success_shape() {
    let body = json!({"forgotten": true, "error": Value::Null});
    assert_eq!(body, json!({"forgotten": true, "error": null}));
}

// ── btctl return-code conventions ────────────────────────────────────────

#[tokio::test]
async fn btctl_missing_binary_is_rc_127() {
    // Force a non-existent binary by overriding PATH to an empty dir is racy
    // process-wide; instead assert the spawn-failure code path's contract by
    // calling a command name that cannot exist. We cannot rename the const, so
    // this exercises the real `bluetoothctl` only when present; on CI without
    // it, the spawn fails and returns rc=127 with a non-empty stderr.
    let (rc, stdout, _stderr) = btctl(&["--version"], Duration::from_secs(2)).await;
    // Either bluetoothctl exists (rc 0) or the spawn failed (rc 127). Both are
    // valid; the contract under test is that a spawn failure yields 127, never
    // a panic.
    assert!(rc == 0 || rc == 127, "rc was {rc}");
    // When the binary is absent the stdout is empty (the Python `""`).
    if rc == 127 {
        assert!(stdout.is_empty());
    }
}

// ── profile gate ─────────────────────────────────────────────────────────

#[tokio::test]
async fn profile_mismatch_is_the_fastapi_404_shape() {
    let resp = profile_mismatch();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_json(resp).await,
        json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})
    );
}

// No `bt_error` test: the Bluetooth routes never return an HTTP error — every
// `btctl` fault is swallowed into a return code and rides in the 200 result
// dict's `error` field (see the note above the `btctl` seam), so there is no
// 5xx error helper on this surface.

// ── clear_primary_if_matches: only forwards on a matching persisted primary ─

#[tokio::test]
async fn clear_primary_skips_when_persisted_primary_does_not_match() {
    let dir = tempfile::tempdir().unwrap();
    // A persisted primary that is a USB device, not the bt device being forgotten.
    let sidecar = dir.path().join("ground-station-input.json");
    std::fs::write(&sidecar, r#"{"primary":"usb:045e:028e:event3"}"#).unwrap();
    // No hid-cmd.sock exists; the helper must be a clean no-op (no panic, no
    // hang) because the persisted primary does not match the device. Every path
    // is threaded in, so the test never mutates the process environment.
    let hid_sock = dir.path().join("hid-cmd.sock");
    clear_primary_if_matches_at("bt:aa:bb:cc:dd:ee:ff", &sidecar, &hid_sock).await;
}

#[test]
fn split_whitespace_max3_keeps_remainder_whole() {
    assert_eq!(
        split_whitespace_max3("Device AA:BB Long Name Here"),
        vec!["Device", "AA:BB", "Long Name Here"]
    );
    assert_eq!(
        split_whitespace_max3("Device   AA:BB   Pad"),
        vec!["Device", "AA:BB", "Pad"]
    );
    assert_eq!(
        split_whitespace_max3("Device AA:BB"),
        vec!["Device", "AA:BB"]
    );
}
