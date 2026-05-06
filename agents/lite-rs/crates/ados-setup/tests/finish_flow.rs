//! End-to-end coverage of the wizard's terminal step: `POST /finish`
//! and the boot-to-ready transition it triggers.
//!
//! The OpenAPI spec at `proto/setup/setup-api.yaml` defines `/finish`
//! as a body-less POST that mutates persisted state to set
//! `setup_finalized=true` and returns the canonical SetupStatus shape.
//! A subsequent GET on `/status` reflects the new flag, and
//! `next_action` flips from "pair" to "ready". This is the
//! boot-to-ready contract the wizard relies on.
//!
//! Coverage:
//! - Body-less POST → 2xx + state advances to "ready".
//! - JSON body sent against the body-less route → still accepted; the
//!   handler ignores extra bytes (axum allows POST with no Body
//!   extractor to receive arbitrary bytes; the contract is body-less).
//!   This documents the actually-shipped behaviour so a future tightener
//!   has a regression test to update.
//! - Replay (same call twice) is idempotent — second call also returns
//!   2xx, status stays `setup_finalized=true`, no error.
//! - Wrong content-type on the (still-ignored) body does not 415; the
//!   handler does not parse the body, so content-type is irrelevant.
//!   Recorded so a future content-type tightener has a regression
//!   anchor.

use std::sync::Arc;

use ados_setup::{setup_router, state::StateStore, SetupState};
use axum::body::{to_bytes, Body};
use axum::http::{Method, Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

fn fresh_state(dir: &tempfile::TempDir) -> Arc<SetupState> {
    let agent_yaml = dir.path().join("agent.yaml");
    std::fs::write(
        &agent_yaml,
        "agent:\n  device_id: \"finish-flow-001\"\n  name: \"Finish Flow\"\nmavlink:\n  port: \"/dev/ttyS0\"\n  baud: 115200\ncloud:\n  api_key: \"\"\napi:\n  bind: \"127.0.0.1:18080\"\n",
    )
    .unwrap();
    let state_path = dir.path().join("setup-state.json");
    let store = StateStore::new(state_path);
    let store_for_status = store.clone();
    Arc::new(SetupState {
        agent_yaml,
        store,
        status_builder: Box::new(move || {
            let persisted = store_for_status.load().unwrap_or_default();
            json!({
                "version": "0.1.0",
                "agent_version": "0.1.0",
                "device_id": "finish-flow-001",
                "device_name": "Finish Flow",
                "profile": "drone",
                "ground_role": "",
                "runtime_mode": "lite",
                "setup_complete": persisted.finalized,
                "setup_finalized": persisted.finalized,
                "completion_percent": if persisted.finalized { 100 } else { 0 },
                "next_action": if persisted.finalized { "ready" } else { "pair" },
                "steps": [],
                "skipped_steps": persisted.skipped_steps.iter().cloned().collect::<Vec<_>>(),
                "access_urls": [],
                "network": { "hostname": "", "mdns_host": "", "api_port": 8080,
                             "hotspot_enabled": false, "hotspot_ssid": "", "local_ips": [] },
                "mavlink": { "connected": false, "port": "/dev/ttyS0", "baud": 115200,
                             "websocket_url": null, "public_websocket_url": null },
                "video": { "state": "not_initialized", "whep_url": null,
                           "public_whep_url": null, "recording": false },
                "remote_access": { "provider": "none", "enabled": false, "configured": false,
                                   "status": "disabled", "public_urls": [], "error": "" },
                "cloud_choice": { "mode": "cloud", "paired": false, "pair_code_required": true,
                                  "backend_url": "", "backend_reachable": false,
                                  "last_checked": null },
                "profile_suggestion": null,
                "hardware_check": null,
                "services": []
            })
        }),
    })
}

/// One-shot helper. Returns (status, body-as-Value).
async fn drive(
    state: Arc<SetupState>,
    method: Method,
    path: &str,
    body: Option<(&str, Vec<u8>)>,
) -> (StatusCode, Value) {
    let router = setup_router(state);
    let mut builder = Request::builder().method(method).uri(path);
    let request = match body {
        Some((ct, raw)) => {
            builder = builder.header("content-type", ct);
            builder.body(Body::from(raw)).unwrap()
        }
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let value: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

#[tokio::test]
async fn finish_with_no_body_advances_to_ready() {
    // Spec: POST /finish has no requestBody. The handler reads no
    // body extractor and unconditionally calls store.mark_finalized().
    // The status snapshot returned in the response reflects the new
    // setup_finalized=true flag.
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let (status, body) = drive(
        state.clone(),
        Method::POST,
        "/api/v1/setup/finish",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["setup_finalized"], true);
    assert_eq!(body["completion_percent"], 100);
    assert_eq!(body["next_action"], "ready");

    // GET /status confirms the persisted transition is visible to a
    // subsequent reader.
    let (status, body) = drive(
        state,
        Method::GET,
        "/api/v1/setup/status",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["setup_finalized"], true);
    assert_eq!(body["next_action"], "ready");
}

#[tokio::test]
async fn finish_replay_is_idempotent() {
    // Operators can double-tap "Finish" or have the wizard auto-retry on
    // a flaky network. A second POST must not error and must not regress
    // the persisted state. Both calls return 200 with the same shape.
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let (s1, b1) = drive(
        state.clone(),
        Method::POST,
        "/api/v1/setup/finish",
        None,
    )
    .await;
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(b1["setup_finalized"], true);
    let (s2, b2) = drive(
        state.clone(),
        Method::POST,
        "/api/v1/setup/finish",
        None,
    )
    .await;
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(b2["setup_finalized"], true);
    assert_eq!(b2["next_action"], "ready");

    // The persisted state file on disk is also still finalized after
    // two calls.
    let store_path = state
        .agent_yaml
        .parent()
        .unwrap()
        .join("setup-state.json");
    let raw = std::fs::read_to_string(&store_path).unwrap();
    assert!(
        raw.contains("\"setup_finalized\": true") || raw.contains("\"setup_finalized\":true"),
        "persisted state file should hold setup_finalized=true: {raw}"
    );
}

#[tokio::test]
async fn finish_with_form_body_is_accepted_handler_ignores_payload() {
    // The wizard's terminal step in some operator flows historically
    // posts an x-www-form-urlencoded body. The in-tree handler is
    // body-less (it does not declare a Body extractor), so axum drops
    // the body bytes silently. The contract recorded here: the request
    // is accepted with 200 regardless of payload, and the status flips
    // to ready exactly as if no body had been sent.
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let (status, body) = drive(
        state,
        Method::POST,
        "/api/v1/setup/finish",
        Some((
            "application/x-www-form-urlencoded",
            b"confirm=true".to_vec(),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["setup_finalized"], true);
    assert_eq!(body["next_action"], "ready");
}

#[tokio::test]
async fn finish_with_json_body_is_also_ignored_no_415() {
    // Spec records `/finish` as body-less. A wizard build that mistakenly
    // posts JSON should still complete the transition rather than 415,
    // because the handler never tries to parse the body. This test
    // documents the shipped behaviour. A future content-type tightener
    // can replace this assertion with a 415 expectation and update the
    // handler in the same change.
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);
    let (status, body) = drive(
        state,
        Method::POST,
        "/api/v1/setup/finish",
        Some((
            "application/json",
            serde_json::to_vec(&json!({"confirm": true})).unwrap(),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["setup_finalized"], true);
}

#[tokio::test]
async fn finish_then_reset_round_trips_state() {
    // Boot-to-ready must be reversible: an operator who realises they
    // mis-configured the wizard can hit /reset and walk back through
    // the steps. This is the wizard "redo" contract.
    let dir = tempfile::tempdir().unwrap();
    let state = fresh_state(&dir);

    let (s, b) = drive(
        state.clone(),
        Method::POST,
        "/api/v1/setup/finish",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["setup_finalized"], true);

    let (s, b) = drive(
        state.clone(),
        Method::POST,
        "/api/v1/setup/reset",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["setup_finalized"], false);
    assert_eq!(b["next_action"], "pair");

    // Subsequent /status read also confirms the rollback.
    let (_, b) = drive(state, Method::GET, "/api/v1/setup/status", None).await;
    assert_eq!(b["setup_finalized"], false);
}
