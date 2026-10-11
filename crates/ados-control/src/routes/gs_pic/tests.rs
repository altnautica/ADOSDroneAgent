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

fn obj(v: Value) -> Map<String, Value> {
    v.as_object().unwrap().clone()
}

/// Spin a one-shot PIC control socket under a tempdir that reads one request
/// line and replies with `reply`, then runs the handler produced by `run` (which
/// receives the socket path so it can drive [`pic_request_at`] directly).
/// Returns `{request, status, body}` so the test can assert both the op
/// forwarded and the response. The socket path is threaded in, so the test never
/// mutates the process-global `ADOS_RUN_DIR`.
async fn with_socket<F, Fut>(reply: Value, run: F) -> Value
where
    F: FnOnce(std::path::PathBuf) -> Fut,
    Fut: std::future::Future<Output = Response>,
{
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("pic.sock");
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
    drop(dir);
    json!({ "request": request, "status": status, "body": body })
}

// ── claim_body translation ───────────────────────────────────────────────

#[test]
fn claim_body_fresh_drops_ok_and_mode() {
    let reply = obj(json!({
        "ok": true, "claimed": true, "mode": "fresh",
        "claimed_by": "op-a", "claim_counter": 1
    }));
    assert_eq!(
        claim_body(&reply),
        json!({"claimed": true, "claimed_by": "op-a", "claim_counter": 1})
    );
}

#[test]
fn claim_body_idempotent_carries_the_flag() {
    let reply = obj(json!({
        "ok": true, "claimed": true, "mode": "idempotent",
        "claimed_by": "op-a", "claim_counter": 1
    }));
    assert_eq!(
        claim_body(&reply),
        json!({"claimed": true, "claimed_by": "op-a", "claim_counter": 1, "idempotent": true})
    );
}

#[test]
fn claim_body_forced_carries_previous_pic() {
    let reply = obj(json!({
        "ok": true, "claimed": true, "mode": "forced",
        "claimed_by": "op-b", "claim_counter": 2, "previous_pic": "op-a"
    }));
    assert_eq!(
        claim_body(&reply),
        json!({
            "claimed": true, "claimed_by": "op-b", "claim_counter": 2,
            "forced": true, "previous_pic": "op-a"
        })
    );
}

#[test]
fn claim_body_transferred_carries_the_source() {
    let reply = obj(json!({
        "ok": true, "claimed": true, "mode": "transferred",
        "claimed_by": "op-b", "claim_counter": 2, "transferred_from": "op-a"
    }));
    assert_eq!(
        claim_body(&reply),
        json!({
            "claimed": true, "claimed_by": "op-b", "claim_counter": 2,
            "transferred_from": "op-a"
        })
    );
}

#[test]
fn claim_body_soft_reject_maps_to_the_409_dict() {
    let reply = obj(json!({
        "ok": true, "claimed": false, "error": "already_claimed",
        "current_pic": "op-a", "needs_confirm": true, "status": 409
    }));
    assert_eq!(
        claim_body(&reply),
        json!({
            "claimed": false, "error": "already_claimed",
            "current_pic": "op-a", "needs_confirm": true, "status": 409
        })
    );
}

// ── release_body translation ─────────────────────────────────────────────

#[test]
fn a_reply_without_an_outcome_decides_nothing() {
    let no_flag = json!({"ok": true}).as_object().unwrap().clone();
    assert!(!has_outcome(&no_flag, "claimed"));
    let odd = json!({"ok": true, "claimed": "yes"})
        .as_object()
        .unwrap()
        .clone();
    assert!(!has_outcome(&odd, "claimed"));
    let refused = json!({"ok": true, "claimed": false})
        .as_object()
        .unwrap()
        .clone();
    assert!(has_outcome(&refused, "claimed"));
}

#[test]
fn release_body_success_and_reject() {
    let ok = obj(json!({"ok": true, "released": true, "previous_pic": "op-a"}));
    assert_eq!(
        release_body(&ok),
        json!({"released": true, "previous_pic": "op-a"})
    );
    let reject = obj(json!({
        "ok": true, "released": false, "error": "not_current_pic",
        "current_pic": "op-a", "status": 403
    }));
    assert_eq!(
        release_body(&reject),
        json!({
            "released": false, "error": "not_current_pic",
            "current_pic": "op-a", "status": 403
        })
    );
}

// ── heartbeat translation ────────────────────────────────────────────────

#[tokio::test]
async fn heartbeat_ok_returns_the_200_dict() {
    let reply = obj(json!({
        "ok": true, "ok_heartbeat": true, "claimed_by": "op-a",
        "claim_counter": 1, "last_heartbeat_ts": 12.5
    }));
    let resp = heartbeat_response(&reply);
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        body_json(resp).await,
        json!({
            "ok": true, "claimed_by": "op-a",
            "claim_counter": 1, "last_heartbeat_ts": 12.5
        })
    );
}

#[tokio::test]
async fn heartbeat_not_holder_is_a_410_error_object() {
    let reply = obj(json!({
        "ok": true, "ok_heartbeat": false, "error": "no_active_claim",
        "current_pic": "op-a", "status": 410
    }));
    let resp = heartbeat_response(&reply);
    assert_eq!(resp.status(), StatusCode::GONE);
    assert_eq!(
        body_json(resp).await,
        json!({"detail": {"error": {
            "code": "E_PIC_NO_ACTIVE_CLAIM",
            "message": "no_active_claim",
            "current_pic": "op-a",
        }}})
    );
}

// ── error envelopes ──────────────────────────────────────────────────────

#[tokio::test]
async fn pic_error_is_the_error_object_shape() {
    let resp = pic_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "E_PIC_CLAIM_FAILED",
        "PIC control socket unavailable",
    );
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        body_json(resp).await,
        json!({"detail": {"error": {
            "code": "E_PIC_CLAIM_FAILED",
            "message": "PIC control socket unavailable",
        }}})
    );
}

#[tokio::test]
async fn profile_mismatch_is_the_fastapi_404_shape() {
    let resp = profile_mismatch();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_json(resp).await,
        json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})
    );
}

// ── the no-socket degrade arms (the 500 path) ────────────────────────────
//
// These thread an explicit socket path into pic_request_at, so no test mutates
// the process-global ADOS_RUN_DIR. They exercise the unavailable path WITHOUT
// the profile gate (the gate reads the real config and would 404 on a dev
// host); each calls pic_request_at directly so the socket seam is covered
// independent of the gate.

#[tokio::test]
async fn pic_request_with_no_socket_is_unavailable() {
    // An absent socket path under a tempdir → the connect fails → Unavailable.
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("pic.sock");
    let out = pic_request_at(&sock, &json!({"op": "claim", "client_id": "op-a"})).await;
    assert!(matches!(out, PicReply::Unavailable));
}

#[tokio::test]
async fn pic_request_treats_ok_false_as_unavailable() {
    // A transport ok:false (a malformed-request error from the dispatch) is
    // not an arbiter outcome; the route surfaces it as the 500, never a body.
    let out = with_socket(
        json!({"ok": false, "error": "E_MISSING_CLIENT_ID"}),
        |sock| async move {
            match pic_request_at(&sock, &json!({"op": "claim"})).await {
                PicReply::Unavailable => {
                    pic_error(StatusCode::INTERNAL_SERVER_ERROR, "E_PIC_CLAIM_FAILED", "x")
                }
                PicReply::Obj(_) => Json(json!({"unexpected": true})).into_response(),
            }
        },
    )
    .await;
    assert_eq!(out["status"], json!(500));
    assert_eq!(
        out["body"]["detail"]["error"]["code"],
        json!("E_PIC_CLAIM_FAILED")
    );
}

// ── the full forward path against a live mock socket ─────────────────────

#[tokio::test]
async fn claim_forwards_the_op_and_returns_the_fresh_body() {
    let out = with_socket(
            json!({"ok": true, "claimed": true, "mode": "fresh", "claimed_by": "op-a", "claim_counter": 1}),
            |sock| async move {
                // Drive the socket seam + translation directly (the profile gate
                // reads the real config and would 404 on a dev host).
                match pic_request_at(&sock, &json!({
                    "op": "claim", "client_id": "op-a", "confirm_token": null, "force": false
                }))
                .await
                {
                    PicReply::Obj(reply) => Json(claim_body(&reply)).into_response(),
                    PicReply::Unavailable => Json(json!({"unavailable": true})).into_response(),
                }
            },
        )
        .await;
    // The exact op + fields forwarded.
    assert_eq!(out["request"]["op"], json!("claim"));
    assert_eq!(out["request"]["client_id"], json!("op-a"));
    assert_eq!(out["request"]["force"], json!(false));
    assert_eq!(out["status"], json!(200));
    assert_eq!(
        out["body"],
        json!({"claimed": true, "claimed_by": "op-a", "claim_counter": 1})
    );
}

#[tokio::test]
async fn confirm_token_returns_token_and_fixed_ttl() {
    let out = with_socket(
        json!({"ok": true, "token": "a".repeat(32)}),
        |sock| async move {
            match pic_request_at(&sock, &json!({"op": "confirm_token", "client_id": "op-b"})).await
            {
                PicReply::Obj(reply) => {
                    let token = reply
                        .get("token")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    Json(json!({"token": token, "ttl_seconds": 2})).into_response()
                }
                PicReply::Unavailable => Json(json!({"unavailable": true})).into_response(),
            }
        },
    )
    .await;
    assert_eq!(out["request"]["op"], json!("confirm_token"));
    assert_eq!(out["status"], json!(200));
    assert_eq!(out["body"]["ttl_seconds"], json!(2));
    assert_eq!(out["body"]["token"].as_str().unwrap().len(), 32);
}

// ── GET /pic: the live arbiter read ─────────────────────────────────────

/// A claimed arbiter must surface the real holder.
///
/// The route previously answered a hardcoded `unclaimed` regardless, so an
/// operator was shown "no pilot in command" while another client held the
/// sticks and the router was refusing their injection on the basis of the
/// real holder.
#[tokio::test]
async fn pic_read_surfaces_the_live_holder() {
    let out = with_socket(
        json!({
            "ok": true, "state": "claimed", "claimed_by": "op-a",
            "claimed_since": 1_700_000_000.0, "claim_counter": 3,
            "primary_gamepad_id": "usb-0000:00:14.0-2",
        }),
        |sock| async move {
            match pic_request_at(&sock, &json!({"op": "get_state"})).await {
                PicReply::Obj(reply) => Json(pic_state_body(&reply)).into_response(),
                PicReply::Unavailable => pic_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "E_PIC_ARBITER_UNAVAILABLE",
                    "PIC control socket unavailable",
                ),
            }
        },
    )
    .await;
    assert_eq!(out["request"]["op"], json!("get_state"));
    assert_eq!(out["status"], json!(200));
    assert_eq!(
        out["body"],
        json!({
            "state": "claimed",
            "claimed_by": "op-a",
            "claimed_since": 1_700_000_000.0,
            "claim_counter": 3,
            "primary_gamepad_id": "usb-0000:00:14.0-2",
        }),
        "the transport ok is dropped; every arbiter field is relayed verbatim"
    );
}

/// An unreachable arbiter is a 503, NOT an unclaimed body. "We cannot see who
/// has the sticks" must never render as "nobody has the sticks" — taking over
/// on that reading is a mid-flight control conflict.
#[tokio::test]
async fn pic_read_degrades_to_503_never_to_unclaimed() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("pic.sock");
    let resp = match pic_request_at(&sock, &json!({"op": "get_state"})).await {
        PicReply::Obj(reply) => Json(pic_state_body(&reply)).into_response(),
        PicReply::Unavailable => pic_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "E_PIC_ARBITER_UNAVAILABLE",
            "PIC control socket unavailable",
        ),
    };
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = body_json(resp).await;
    assert_eq!(
        body["detail"]["error"]["code"],
        json!("E_PIC_ARBITER_UNAVAILABLE")
    );
    assert!(
        body.get("state").is_none(),
        "an unreachable arbiter must report no state at all, got {body}"
    );
}

/// A reply missing a field carries it as an explicit `null` rather than
/// dropping the key: a vanished key reads to a consumer as "not reported" and
/// gets coerced, which is the same defect one layer up in the cloud path.
#[test]
fn pic_state_body_nulls_an_unreported_field_rather_than_omitting_it() {
    let reply = obj(json!({"ok": true, "state": "unclaimed"}));
    let body = pic_state_body(&reply);
    assert_eq!(
        body,
        json!({
            "state": "unclaimed",
            "claimed_by": null,
            "claimed_since": null,
            "claim_counter": null,
            "primary_gamepad_id": null,
        })
    );
}
