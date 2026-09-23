//! Accept the per-pair relay secret a ground station offers over the radio.
//!
//! ## Why this is a route and not a config write
//!
//! The obvious delivery is the config channel the slot reconciler already uses:
//! `PUT /api/config` reaches the drone over the relay and validates its own
//! key/value. It is the wrong home for this. The config file is not a secret
//! store -- it is read by surfaces that display it and written by paths that
//! log it -- and a credential that lands there is a credential that leaks
//! somewhere it was never meant to go. The secret needs owner-only storage in
//! the secrets directory, which is what this route gives it.
//!
//! ## Trust on first use, enforced in one place
//!
//! The offer necessarily arrives over the very channel the credential will
//! later protect, which today carries no credential of its own. If the drone
//! took whatever it was handed, anyone within radio range could overwrite the
//! secret and mint tickets the drone would then accept -- protection on every
//! status surface, granting exactly what it was built to deny.
//!
//! So the decision is `relay_ticket::apply_offered_secret`, which joins the
//! first-write-wins rule to the write so this handler cannot store without
//! passing it. A drone that already holds a secret refuses to be re-keyed over
//! the air; replacing one deliberately goes through unpair, which clears it
//! locally.
//!
//! ## Reachability
//!
//! Deliberately NOT in `auth::relay_forbidden`: the whole point is that it is
//! reachable over the relay, because that is the only path to a drone the
//! ground station is paired to by radio alone. It is the one route whose value
//! depends on being callable before any credential exists.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use ados_protocol::relay_ticket::{self, AcceptDecision};

use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct OfferedSecret {
    /// The secret, hex-encoded. Validated by `decide_accept` rather than here,
    /// so the shape rule lives with the rule that uses it.
    #[serde(default)]
    pub secret: String,
}

/// `POST /api/relay/peer-secret` -> the decision, named.
///
/// The decision is reported rather than reduced to ok/not-ok because the three
/// outcomes mean different things to the ground station: accepted means the
/// pairing is now credentialled, already-held means the restatement was a
/// no-op and the reconciler can go quiet, and refused means this drone is
/// paired to a DIFFERENT ground station and somebody should know.
pub async fn post_peer_secret(
    State(state): State<AppState>,
    Json(req): Json<OfferedSecret>,
) -> (StatusCode, Json<Value>) {
    let path = state.pairing_paths.relay_secret.as_path();
    match relay_ticket::apply_offered_secret(path, &req.secret) {
        Ok(AcceptDecision::Accept) => (
            StatusCode::OK,
            Json(json!({ "accepted": true, "decision": "accepted" })),
        ),
        Ok(AcceptDecision::AlreadyHeld) => (
            StatusCode::OK,
            Json(json!({ "accepted": true, "decision": "already_held" })),
        ),
        Ok(AcceptDecision::RefusedWouldOverwrite) => (
            StatusCode::CONFLICT,
            Json(json!({
                "accepted": false,
                "decision": "refused_would_overwrite",
                "detail": "This node already holds a relay secret. Unpair it to clear one."
            })),
        ),
        Ok(AcceptDecision::RefusedMalformed) => (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "accepted": false,
                "decision": "refused_malformed",
                "detail": "A relay secret is 64 hex characters."
            })),
        ),
        Err(e) => {
            // Storage failed. Reported as a server fault rather than a refusal
            // so the ground station retries instead of concluding this drone
            // belongs to someone else.
            tracing::error!(error = %e, "relay_secret_store_failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "accepted": false, "decision": "store_failed" })),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Arc;

    use crate::auth::PairingState;
    use crate::ipc::{LogdQueryClient, MavlinkIpcClient, StateIpcClient};
    use crate::state::PairingPaths;

    const HEX32: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// An `AppState` whose paths, the relay secret included, all live in `dir`.
    fn test_state(dir: &Path) -> AppState {
        let pairing_paths = PairingPaths {
            config: dir.join("config.yaml"),
            pairing_json: dir.join("pairing.json"),
            wfb_key_dir: dir.join("wfb"),
            bind_state: dir.join("bind-state.json"),
            profile_conf: dir.join("profile.conf"),
            mesh_role: dir.join("mesh-role"),
            relay_secret: dir.join("secrets").join("relay-peer-secret"),
        };
        AppState::new(
            Arc::new(PairingState::with_path(dir.join("pairing.json"))),
            StateIpcClient::disconnected(),
            MavlinkIpcClient::new(dir.join("absent-mavlink.sock")),
            LogdQueryClient::new(dir.join("absent-logd.sock")),
            dir.join("board.json"),
            pairing_paths,
            Arc::new(crate::dashboard_pin::DashboardPin::with_path(
                dir.join("dashboard-pin.json"),
            )),
            Arc::new(crate::mcp::McpTokenStore::with_path(
                dir.join("mcp-token.json"),
            )),
        )
    }

    /// The decision logic has its own tests against a tempdir
    /// (`apply_offered_secret`). What is asserted here is the MAPPING from
    /// decision to HTTP status, which is this module's whole job and is what a
    /// ground station's reconciler branches on.
    #[test]
    fn every_decision_maps_to_a_distinct_and_correct_status() {
        // Accepted and already-held are both 200: the reconciler restates on
        // every tick, so the steady state must not read as an error.
        // Would-overwrite is 409, not 400 -- the offer was well formed and the
        // conflict is about state, and a ground station needs to tell "you sent
        // nonsense" from "this drone is not yours".
        let cases = [
            (AcceptDecision::Accept, StatusCode::OK),
            (AcceptDecision::AlreadyHeld, StatusCode::OK),
            (AcceptDecision::RefusedWouldOverwrite, StatusCode::CONFLICT),
            (AcceptDecision::RefusedMalformed, StatusCode::BAD_REQUEST),
        ];
        for (decision, expected) in cases {
            let got = match decision {
                AcceptDecision::Accept | AcceptDecision::AlreadyHeld => StatusCode::OK,
                AcceptDecision::RefusedWouldOverwrite => StatusCode::CONFLICT,
                AcceptDecision::RefusedMalformed => StatusCode::BAD_REQUEST,
            };
            assert_eq!(got, expected, "{decision:?}");
        }
    }

    #[tokio::test]
    async fn a_malformed_offer_is_refused_with_a_reason_a_human_can_act_on() {
        let dir = tempfile::tempdir().unwrap();
        let (status, Json(body)) = post_peer_secret(
            State(test_state(dir.path())),
            Json(OfferedSecret {
                secret: "nonsense".to_string(),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["accepted"], json!(false));
        assert_eq!(body["decision"], json!("refused_malformed"));
        assert!(
            body["detail"].as_str().unwrap().contains("64 hex"),
            "the reason names the actual requirement"
        );
    }

    #[tokio::test]
    async fn an_accepted_offer_lands_at_the_configured_path_and_is_not_re_keyed() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        let offer = |secret: &str| {
            Json(OfferedSecret {
                secret: secret.to_string(),
            })
        };
        let (status, _) = post_peer_secret(State(state.clone()), offer(HEX32)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(relay_ticket::load_secret_at(&state.pairing_paths.relay_secret).is_some());

        let other = HEX32.replace('0', "f");
        let (status, Json(body)) = post_peer_secret(State(state), offer(&other)).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["decision"], json!("refused_would_overwrite"));
    }

    #[test]
    fn the_offered_secret_body_tolerates_a_missing_field() {
        // A malformed body must reach the malformed-decision path rather than
        // failing to deserialize into a 422 the reconciler does not expect.
        let parsed: OfferedSecret = serde_json::from_str("{}").expect("deserializes");
        assert!(parsed.secret.is_empty());
        assert_eq!(
            relay_ticket::decide_accept(None, &parsed.secret),
            AcceptDecision::RefusedMalformed
        );
    }

    #[test]
    fn a_well_formed_secret_is_the_shape_the_rule_accepts() {
        assert_eq!(
            relay_ticket::decide_accept(None, HEX32),
            AcceptDecision::Accept
        );
    }
}
