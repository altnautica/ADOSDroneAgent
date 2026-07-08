//! Dashboard-access PIN routes: `/api/dashboard/pin/{status,verify,set,clear}`.
//!
//! A paired agent's own web dashboard, reached from off-box (a phone/laptop on
//! the LAN pasting `http://<node-ip>:8080`), has no `X-ADOS-Key`. Instead of the
//! old raw-key prompt, the dashboard shows a branded PIN splash: the first LAN
//! visitor SETS a PIN (trust-on-first-use, the same posture the pairing-claim
//! flow takes), a returning visitor ENTERS it, and a correct PIN mints a scoped
//! [`ados_protocol::dashboard_session`] token the browser then sends on every
//! `/api/*` call (the front accepts it alongside `X-ADOS-Key`).
//!
//! Auth posture per route (the edge public-exempts status/verify/set so an
//! off-box paired browser can reach them; clear stays behind the normal gate):
//!
//! - **status** — public read of `{pin_set, locked, locked_until}` (booleans, no
//!   secret) so the splash picks set-vs-enter.
//! - **verify** — public login: rate-limited by the shared limiter + the in-store
//!   lockout ladder. A correct PIN returns a session.
//! - **set** — public at the edge, AUTHORIZED IN THE HANDLER: on-box OR a valid
//!   `X-ADOS-Key` OR a valid current session OR trust-on-first-use (`!pin_set`)
//!   OR a matching `current_pin`. A change with none of those is `403`.
//! - **clear** — NOT public: the normal gate already admits only on-box or a
//!   valid credential (the GCS holds the key), which is exactly reset's audience.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use ados_protocol::dashboard_session::DashboardSession;
use ados_protocol::pairing_posture::{constant_time_eq, Pairing};

use crate::dashboard_pin::{PinStatus, VerifyOutcome, DASHBOARD_SESSION_HEADER};
use crate::routes::detail;
use crate::serve::ONBOX_HEADER;
use crate::state::AppState;

/// `GET /api/dashboard/pin/status` → `{pin_set, locked, locked_until|null}`.
/// Public — reveals only booleans so the splash can pick set-vs-enter.
pub async fn get_pin_status(State(state): State<AppState>) -> Json<Value> {
    let PinStatus {
        pin_set,
        locked,
        locked_until,
    } = state.dashboard_pin.status(now_unix_seconds());
    Json(json!({
        "pin_set": pin_set,
        "locked": locked,
        // Only meaningful while locked; null otherwise so the client does not
        // render a stale countdown.
        "locked_until": if locked { json!(locked_until) } else { Value::Null },
    }))
}

/// The `POST /api/dashboard/pin/verify` body.
#[derive(Deserialize)]
pub struct VerifyRequest {
    pub pin: String,
}

/// `POST /api/dashboard/pin/verify` — enter the PIN. On success returns a session
/// token; on a wrong PIN a `401` with the remaining-attempt countdown; while
/// locked a `429` with the lockout expiry.
pub async fn verify_pin(State(state): State<AppState>, Json(req): Json<VerifyRequest>) -> Response {
    match state.dashboard_pin.verify_pin(&req.pin, now_unix_seconds()) {
        VerifyOutcome::Ok => session_response(&state),
        VerifyOutcome::Wrong { remaining_attempts } => (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "ok": false, "remaining_attempts": remaining_attempts })),
        )
            .into_response(),
        VerifyOutcome::Locked { locked_until } => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({ "ok": false, "locked_until": locked_until })),
        )
            .into_response(),
        VerifyOutcome::NotSet => detail(StatusCode::CONFLICT, "No dashboard PIN is set"),
    }
}

/// The `POST /api/dashboard/pin/set` body. `current_pin` is only consulted for a
/// CHANGE (a PIN is already set and the caller has no other authorization).
#[derive(Deserialize)]
pub struct SetRequest {
    pub pin: String,
    #[serde(default)]
    pub current_pin: Option<String>,
}

/// `POST /api/dashboard/pin/set` — set or change the PIN. See the module note for
/// the handler-side authorization set. On success returns a fresh session (so the
/// setter is immediately unlocked).
pub async fn set_pin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SetRequest>,
) -> Response {
    let now = now_unix_seconds();
    let pairing = state.pairing.current();

    // The edge stamps a trustworthy on-box header (stripped-then-set) — the local
    // operator at the device.
    let on_box = header(&headers, ONBOX_HEADER).as_deref() == Some("1");
    // A valid pairing key (the GCS holds it).
    let key_valid = match (&pairing, header(&headers, "x-ados-key")) {
        (Pairing::Paired(k), Some(key)) => constant_time_eq(key.as_bytes(), k.as_bytes()),
        _ => false,
    };
    // A valid current dashboard session (a browser already unlocked).
    let session_valid = header(&headers, DASHBOARD_SESSION_HEADER)
        .map(|t| state.dashboard_pin.session_valid_for(&pairing, &t))
        .unwrap_or(false);
    let pin_set = state.dashboard_pin.is_set();

    // Trust-on-first-use, then the current-PIN change path (short-circuited so a
    // wrong current_pin is only consulted when nothing else authorized, and it
    // counts as a failed attempt against the lockout ladder).
    let authorized = on_box
        || key_valid
        || session_valid
        || !pin_set
        || req
            .current_pin
            .as_deref()
            .map(|cp| matches!(state.dashboard_pin.verify_pin(cp, now), VerifyOutcome::Ok))
            .unwrap_or(false);

    if !authorized {
        return detail(
            StatusCode::FORBIDDEN,
            "A dashboard PIN is already set. Enter the current PIN, or reset it from Mission Control or on the device.",
        );
    }

    match state.dashboard_pin.set_pin(&req.pin, now) {
        Ok(()) => session_response(&state),
        Err(crate::dashboard_pin::PinError::InvalidPin) => {
            detail(StatusCode::BAD_REQUEST, "PIN must be 4 to 12 digits")
        }
        Err(e) => {
            tracing::error!(error = %e, "dashboard PIN set failed");
            detail(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to set dashboard PIN",
            )
        }
    }
}

/// `POST /api/dashboard/pin/clear` — reset the PIN. Gated by the normal edge auth
/// (on-box or a valid credential), so only an authorized caller reaches this.
/// Rotating the salt on the next set revokes every live session; until then the
/// dashboard re-enters the trust-on-first-use flow.
pub async fn clear_pin(State(state): State<AppState>) -> Response {
    match state.dashboard_pin.clear() {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "dashboard PIN clear failed");
            detail(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to reset dashboard PIN",
            )
        }
    }
}

/// Mint a session under the current pairing key + the (just-written) salt and
/// return `{ok, session, expires_at}`. When unpaired the data plane is open so
/// the session is never verified; minting under the empty-key issuer keeps the
/// response shape valid without special-casing it (mirrors the ws-ticket mint).
fn session_response(state: &AppState) -> Response {
    let api_key = match state.pairing.current() {
        Pairing::Paired(k) => k,
        Pairing::Unpaired => String::new(),
    };
    match state.dashboard_pin.mint_session(&api_key) {
        Some(DashboardSession { token, expires_at }) => Json(json!({
            "ok": true,
            "session": token,
            "expires_at": expires_at,
        }))
        .into_response(),
        // The salt is present immediately after a successful set/verify, so this
        // is a should-not-happen guard rather than an expected path.
        None => detail(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to mint dashboard session",
        ),
    }
}

/// A header value as an owned `String`, `None` when absent or non-UTF-8.
fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// Wall-clock unix seconds (fractional), matching the pairing writers' stamp.
fn now_unix_seconds() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}
