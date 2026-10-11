use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;

/// Read a response body as JSON.
async fn body_json(resp: Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Spin a one-shot Wi-Fi command socket under a tempdir that reads one request
/// line and replies with `reply`, then runs the handler produced by `run` (which
/// receives the socket path so it can drive the handler `_at` core directly).
/// Returns `{request, status, body}` so the test can assert both the op
/// forwarded and the response. The socket path is threaded in, so the test never
/// mutates the process-global `ADOS_RUN_DIR`.
async fn with_socket<F, Fut>(reply: Value, run: F) -> Value
where
    F: FnOnce(std::path::PathBuf) -> Fut,
    Fut: std::future::Future<Output = Response>,
{
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("wifi-cmd.sock");
    let listener = UnixListener::bind(&sock).unwrap();

    let server = tokio::spawn(async move {
        let (mut conn, _addr) = listener.accept().await.unwrap();
        let mut raw = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            let n = conn.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&buf[..n]);
            if raw.contains(&b'\n') {
                break;
            }
        }
        let line: Vec<u8> = raw.split(|&b| b == b'\n').next().unwrap().to_vec();
        let request: Value = serde_json::from_slice(&line).unwrap();
        let mut body = serde_json::to_vec(&reply).unwrap();
        body.push(b'\n');
        conn.write_all(&body).await.unwrap();
        conn.flush().await.unwrap();
        request
    });

    let resp = run(sock.clone()).await;
    let request = server.await.unwrap();
    let status = resp.status().as_u16();
    let body = body_json(resp).await;
    // The temp dir + listener drop here, closing the socket.
    drop(dir);
    json!({ "request": request, "status": status, "body": body })
}

// ── classify_reply ───────────────────────────────────────────────────────

#[test]
fn classify_strips_ok_on_a_success_reply() {
    match classify_reply(r#"{"ok":true,"joined":true,"ip":"1.2.3.4"}"#) {
        NetCmd::Reply(m) => {
            assert!(!m.contains_key("ok"), "the transport ok flag is stripped");
            assert_eq!(m["joined"], json!(true));
            assert_eq!(m["ip"], json!("1.2.3.4"));
        }
        _ => panic!("expected a stripped Reply"),
    }
}

#[test]
fn classify_surfaces_the_error_on_ok_false() {
    match classify_reply(r#"{"ok":false,"error":"E_MISSING_SSID"}"#) {
        NetCmd::Error(msg) => assert_eq!(msg, "E_MISSING_SSID"),
        _ => panic!("expected an Error"),
    }
    // An ok:false with no error falls back to the generic message.
    match classify_reply(r#"{"ok":false}"#) {
        NetCmd::Error(msg) => assert_eq!(msg, "unknown wifi command error"),
        _ => panic!("expected an Error"),
    }
}

#[test]
fn classify_treats_a_non_object_or_garbage_reply_as_unavailable() {
    assert!(matches!(classify_reply("not json"), NetCmd::Unavailable));
    assert!(matches!(classify_reply("[1,2,3]"), NetCmd::Unavailable));
}

#[test]
fn classify_treats_an_absent_ok_as_a_success_reply() {
    // The FastAPI client only branches on `ok is False`; a reply with no ok
    // proceeds to strip-ok (a no-op here) and returns the body.
    match classify_reply(r#"{"forgot":true,"name":"Net"}"#) {
        NetCmd::Reply(m) => {
            assert_eq!(m["forgot"], json!(true));
            assert_eq!(m["name"], json!("Net"));
        }
        _ => panic!("expected a Reply"),
    }
}

// ── join_response ────────────────────────────────────────────────────────

#[test]
fn join_response_pins_the_success_body_shape() {
    let reply: Map<String, Value> = serde_json::from_value(json!({
        "joined": true,
        "ip": "192.168.1.50",
        "gateway": "192.168.1.1",
        "error": Value::Null,
    }))
    .unwrap();
    assert_eq!(
        join_response(&reply),
        json!({
            "joined": true,
            "ip": "192.168.1.50",
            "gateway": "192.168.1.1",
            "error": null,
        })
    );
}

#[test]
fn join_response_defaults_every_field_when_absent() {
    let reply = Map::new();
    assert_eq!(
        join_response(&reply),
        json!({"joined": false, "ip": null, "gateway": null, "error": null})
    );
}

// ── error envelopes ──────────────────────────────────────────────────────

#[tokio::test]
async fn the_unavailable_503_carries_the_nested_error_object() {
    let resp = socket_unavailable("E_WIFI_JOIN_FAILED");
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = body_json(resp).await;
    assert_eq!(
        body,
        json!({"detail": {"error": {
            "code": "E_WIFI_JOIN_FAILED",
            "message": "Wi-Fi command socket unavailable",
        }}})
    );
}

// ── the handlers against a no-socket seam (the 503 path) ─────────────────
//
// Each threads an absent socket path (under a tempdir) into the handler `_at`
// core, so no test mutates the process-global ADOS_RUN_DIR.

#[tokio::test]
async fn join_with_no_socket_is_a_503() {
    // An absent socket path → the connect fails fast → 503.
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("wifi-cmd.sock");
    let resp = put_client_join_at(
        &sock,
        WifiJoinRequest {
            ssid: "Net".to_string(),
            passphrase: Some("pw".to_string()),
            force: None,
        },
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = body_json(resp).await;
    assert_eq!(body["detail"]["error"]["code"], json!("E_WIFI_JOIN_FAILED"));
}

#[tokio::test]
async fn leave_with_no_socket_is_a_503() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("wifi-cmd.sock");
    let resp = delete_client_at(&sock).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = body_json(resp).await;
    assert_eq!(
        body["detail"]["error"]["code"],
        json!("E_WIFI_LEAVE_FAILED")
    );
}

#[tokio::test]
async fn forget_with_no_socket_is_a_503() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("wifi-cmd.sock");
    let resp = delete_client_configured_at(&sock, "Net".to_string()).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = body_json(resp).await;
    assert_eq!(
        body["detail"]["error"]["code"],
        json!("E_WIFI_FORGET_FAILED")
    );
}

// ── the handlers against a live mock socket (the full forward path) ───────

#[tokio::test]
async fn join_forwards_a_wifi_join_op_and_returns_the_success_body() {
    let out = with_socket(
        json!({"ok": true, "joined": true, "ip": "10.0.0.5", "gateway": "10.0.0.1", "error": null}),
        |sock| async move {
            put_client_join_at(
                &sock,
                WifiJoinRequest {
                    ssid: "HomeNet".to_string(),
                    passphrase: Some("secret".to_string()),
                    force: Some(true),
                },
            )
            .await
        },
    )
    .await;
    // The exact op + fields forwarded to the daemon.
    assert_eq!(out["request"]["op"], json!("wifi_join"));
    assert_eq!(out["request"]["ssid"], json!("HomeNet"));
    assert_eq!(out["request"]["passphrase"], json!("secret"));
    assert_eq!(out["request"]["force"], json!(true));
    // The success body shape.
    assert_eq!(out["status"], json!(200));
    assert_eq!(
        out["body"],
        json!({"joined": true, "ip": "10.0.0.5", "gateway": "10.0.0.1", "error": null})
    );
}

#[tokio::test]
async fn join_maps_the_ap_busy_result_to_a_409() {
    let out = with_socket(
        json!({"ok": true, "joined": false, "error": "station_busy_ap_active"}),
        |sock| async move {
            put_client_join_at(
                &sock,
                WifiJoinRequest {
                    ssid: "HomeNet".to_string(),
                    passphrase: None,
                    force: None,
                },
            )
            .await
        },
    )
    .await;
    // The default force is forwarded as false.
    assert_eq!(out["request"]["force"], json!(false));
    assert_eq!(out["status"], json!(409));
    assert_eq!(
        out["body"]["detail"]["error"]["code"],
        json!("E_WLAN0_BUSY_AP_ACTIVE")
    );
    assert_eq!(out["body"]["needs_force"], json!(true));
    // The default hint when the reply carries none.
    assert_eq!(
        out["body"]["detail"]["error"]["message"],
        json!("AP is active; retry with force=true to steal wlan0")
    );
}

#[tokio::test]
async fn join_surfaces_an_ok_false_reply_as_a_500() {
    let out = with_socket(
        json!({"ok": false, "error": "E_MISSING_SSID"}),
        |sock| async move {
            put_client_join_at(
                &sock,
                WifiJoinRequest {
                    ssid: "x".to_string(),
                    passphrase: None,
                    force: None,
                },
            )
            .await
        },
    )
    .await;
    assert_eq!(out["status"], json!(500));
    assert_eq!(
        out["body"]["detail"]["error"]["code"],
        json!("E_WIFI_JOIN_FAILED")
    );
    assert_eq!(
        out["body"]["detail"]["error"]["message"],
        json!("E_MISSING_SSID")
    );
}

#[tokio::test]
async fn leave_forwards_a_wifi_leave_op_and_returns_the_reply() {
    let out = with_socket(
        json!({"ok": true, "left": true, "previous_ssid": "HomeNet"}),
        |sock| async move { delete_client_at(&sock).await },
    )
    .await;
    assert_eq!(out["request"]["op"], json!("wifi_leave"));
    assert_eq!(out["status"], json!(200));
    // The ok flag is stripped; the manager body is returned verbatim.
    assert_eq!(
        out["body"],
        json!({"left": true, "previous_ssid": "HomeNet"})
    );
}

#[tokio::test]
async fn forget_forwards_a_wifi_forget_op_and_returns_the_reply() {
    let out = with_socket(
        json!({"ok": true, "forgot": true, "name": "HomeNet", "error": null}),
        |sock| async move { delete_client_configured_at(&sock, "HomeNet".to_string()).await },
    )
    .await;
    assert_eq!(out["request"]["op"], json!("wifi_forget"));
    assert_eq!(out["request"]["name"], json!("HomeNet"));
    assert_eq!(out["status"], json!(200));
    assert_eq!(
        out["body"],
        json!({"forgot": true, "name": "HomeNet", "error": null})
    );
}

#[tokio::test]
async fn forget_maps_a_failed_forget_to_a_400() {
    let out = with_socket(
        json!({"ok": true, "forgot": false, "name": "HomeNet", "error": "nmcli_failed"}),
        |sock| async move { delete_client_configured_at(&sock, "HomeNet".to_string()).await },
    )
    .await;
    assert_eq!(out["status"], json!(400));
    assert_eq!(
        out["body"]["detail"]["error"]["code"],
        json!("E_WIFI_FORGET_FAILED")
    );
    assert_eq!(
        out["body"]["detail"]["error"]["message"],
        json!("nmcli_failed")
    );
}

#[test]
fn json_truthy_matches_python_bool() {
    assert!(!json_truthy(&Value::Null));
    assert!(json_truthy(&json!(true)));
    assert!(!json_truthy(&json!(false)));
    assert!(!json_truthy(&json!("")));
    assert!(json_truthy(&json!("x")));
}

// ── autoconnect: no-socket 503 + the full forward path ───────────────────

#[tokio::test]
async fn autoconnect_with_no_socket_is_a_503() {
    // An absent socket path → the connect fails fast → 503.
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("wifi-cmd.sock");
    let resp = put_client_autoconnect_at(
        &sock,
        "HomeNet".to_string(),
        AutoconnectRequest {
            enabled: Some(true),
        },
    )
    .await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = body_json(resp).await;
    assert_eq!(
        body["detail"]["error"]["code"],
        json!("E_WIFI_AUTOCONNECT_FAILED")
    );
}

#[tokio::test]
async fn autoconnect_forwards_the_op_and_returns_the_reply() {
    let out = with_socket(
        json!({"ok": true, "autoconnect": true, "name": "HomeNet", "error": null}),
        |sock| async move {
            put_client_autoconnect_at(
                &sock,
                "HomeNet".to_string(),
                AutoconnectRequest {
                    enabled: Some(true),
                },
            )
            .await
        },
    )
    .await;
    // The exact op + fields forwarded to the daemon.
    assert_eq!(out["request"]["op"], json!("wifi_autoconnect"));
    assert_eq!(out["request"]["name"], json!("HomeNet"));
    assert_eq!(out["request"]["enabled"], json!(true));
    // The success body is the manager reply with the ok flag stripped.
    assert_eq!(out["status"], json!(200));
    assert_eq!(
        out["body"],
        json!({"autoconnect": true, "name": "HomeNet", "error": null})
    );
}

#[tokio::test]
async fn autoconnect_defaults_a_missing_enabled_to_false() {
    let out = with_socket(
        json!({"ok": true, "autoconnect": false, "name": "HomeNet", "error": null}),
        |sock| async move {
            put_client_autoconnect_at(
                &sock,
                "HomeNet".to_string(),
                AutoconnectRequest { enabled: None },
            )
            .await
        },
    )
    .await;
    // A missing `enabled` forwards as false (Python `bool(body.get("enabled"))`).
    assert_eq!(out["request"]["enabled"], json!(false));
    assert_eq!(out["status"], json!(200));
}

#[tokio::test]
async fn autoconnect_maps_a_manager_error_to_a_400() {
    let out = with_socket(
            json!({"ok": true, "autoconnect": true, "name": "Nope", "error": "unknown connection 'Nope'"}),
            |sock| async move {
                put_client_autoconnect_at(
                    &sock,
                    "Nope".to_string(),
                    AutoconnectRequest {
                        enabled: Some(true),
                    },
                )
                .await
            },
        )
        .await;
    // A truthy `error` on the reply → the FastAPI 400 carrying the message.
    assert_eq!(out["status"], json!(400));
    assert_eq!(
        out["body"]["detail"]["error"]["code"],
        json!("E_WIFI_AUTOCONNECT_FAILED")
    );
    assert_eq!(
        out["body"]["detail"]["error"]["message"],
        json!("unknown connection 'Nope'")
    );
}

#[tokio::test]
async fn autoconnect_surfaces_an_ok_false_reply_as_a_500() {
    let out = with_socket(
        json!({"ok": false, "error": "E_MISSING_NAME"}),
        |sock| async move {
            put_client_autoconnect_at(
                &sock,
                "".to_string(),
                AutoconnectRequest {
                    enabled: Some(true),
                },
            )
            .await
        },
    )
    .await;
    assert_eq!(out["status"], json!(500));
    assert_eq!(
        out["body"]["detail"]["error"]["code"],
        json!("E_WIFI_AUTOCONNECT_FAILED")
    );
}
