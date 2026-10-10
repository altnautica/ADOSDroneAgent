//! Drone-side attention profile: **`GET`, `POST` and `DELETE /api/video/profile`**.
//!
//! ```text
//! POST   {"profile":"hero"|"thumbnail"}                  plain set: moves the base
//! POST   {"profile":"hero","holder":"<id>","lease_s":N}  create / renew a hero lease
//! POST   {"profile":"thumbnail","holder":"<id>"}         release that holder's lease
//! DELETE ?holder=<id>                                     release (unknown holder: 200)
//! GET                                                     current state
//! ```
//!
//! The effective profile is `hero` while any lease is live, else the base
//! profile: the boot profile, or the last plain set (which is what the ground
//! station's fleet-hero assignment sends). A lease lets an operator watch a
//! thumbnail drone at full rate without moving the fleet's hero, and it lapses
//! `lease_s` seconds after its last renewal, so a viewer that goes away cannot
//! leave a drone on hero. `holder` is 1-64 characters of `[A-Za-z0-9._:-]`
//! (`vision` is reserved for the on-board vision tap's own lease); `lease_s` is
//! an integer from 5 to 120 and defaults to 30. Anything else is a `400`.
//!
//! Every method answers `200` with the state the encoder ACTUALLY applied, not
//! the one that was asked for:
//!
//! ```text
//! {"profile":"hero","base_profile":"thumbnail","width":1280,"height":720,
//!  "fps":30,"bitrate_kbps":4000,"ceiling_kbps":null,"ceiling_deferred":false,
//!  "encoder_respawns":2,"leases":[{"holder":"gcs-1","expires_in_s":29.9}]}
//! ```
//!
//! The whole fleet shares one 20 MHz channel and one radio per node, so exactly
//! one drone streams full video (`hero`, 1280x720/30 fps/4000 kbps) and every
//! other registered drone streams 320x180/1 fps/50 kbps. The airtime arithmetic
//! that fixes those numbers, and the reason 24 drones do NOT fit at MCS 1, lives
//! on `ados_video::profile`.
//!
//! This route is a thin REST surface over `ados-video`'s encoder command socket
//! — the one entry point both this route and the adaptive-bitrate ladder in
//! `ados-radio` drive, so a hero switch, a lease and a link-quality clamp
//! compose instead of clobbering one another. `ados-video` owns the encoder and
//! the lease expiry; this front only asks. A rig with no video pipeline (the
//! socket is absent, or the service is down) answers `503` rather than
//! pretending the request took. A ground station answers `404
//! E_PROFILE_MISMATCH` on every method.
//!
//! The plain set normally arrives over the radio's relay-proxy from the ground
//! station's fleet-hero route, but every method is a first-class local route
//! too: a bench operator can promote a drone over LAN with no ground station in
//! the path.

use std::collections::HashMap;
use std::fmt::Display;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use ados_video::profile::lease::{check_lease_holder, lease_seconds_in_range, LEASE_DEFAULT_S};
use ados_video::profile::{EncoderState, VideoProfile};

use crate::routes::detail;
use crate::state::AppState;

/// A validated `POST /api/video/profile` body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProfileRequest {
    /// No holder: move the base profile.
    Set(VideoProfile),
    /// `hero` with a holder: create or renew that holder's lease.
    Lease { holder: String, lease_s: u64 },
    /// `thumbnail` with a holder: release that holder's lease.
    Release { holder: String },
}

/// `GET /api/video/profile` — this drone's current attention state.
pub async fn get_video_profile(State(state): State<AppState>) -> Response {
    if is_ground_station(&state) {
        return profile_mismatch();
    }
    respond(
        ados_video::profile::state().await,
        "video pipeline state is unavailable",
    )
}

/// `POST /api/video/profile` — set the base profile, or create, renew or
/// release a hero lease.
pub async fn post_video_profile(
    State(state): State<AppState>,
    body: Option<Json<Value>>,
) -> Response {
    // A ground station has no air-side encoder to retarget; the same
    // profile-mismatch shape every profile-gated route uses.
    if is_ground_station(&state) {
        return profile_mismatch();
    }

    let body = body.map(|Json(v)| v).unwrap_or(Value::Null);
    let request = match parse_post(&body) {
        Ok(request) => request,
        Err(message) => return detail(StatusCode::BAD_REQUEST, message),
    };
    let result = match &request {
        ProfileRequest::Set(profile) => ados_video::profile::set_profile(*profile).await,
        ProfileRequest::Lease { holder, lease_s } => {
            ados_video::profile::set_lease(holder, *lease_s).await
        }
        ProfileRequest::Release { holder } => ados_video::profile::release_lease(holder).await,
    };
    if let Err(e) = &result {
        tracing::warn!(error = %e, request = ?request, "video_profile_set_failed");
    }
    respond(result, "video pipeline did not accept the profile change")
}

/// `DELETE /api/video/profile?holder=<id>` — release that holder's lease.
/// Idempotent: a holder that holds nothing still answers `200`.
pub async fn delete_video_profile(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if is_ground_station(&state) {
        return profile_mismatch();
    }
    let holder = match parse_release_query(&query) {
        Ok(holder) => holder,
        Err(message) => return detail(StatusCode::BAD_REQUEST, message),
    };
    let result = ados_video::profile::release_lease(&holder).await;
    if let Err(e) = &result {
        tracing::warn!(error = %e, holder = %holder, "video_profile_release_failed");
    }
    respond(result, "video pipeline did not accept the lease release")
}

/// Validate a `POST` body. The error is the `400` detail.
pub(crate) fn parse_post(body: &Value) -> Result<ProfileRequest, String> {
    let Some(profile) = body
        .get("profile")
        .and_then(Value::as_str)
        .and_then(VideoProfile::parse)
    else {
        return Err("profile must be \"hero\" or \"thumbnail\"".to_string());
    };
    let holder = match body.get("holder") {
        None | Some(Value::Null) => return Ok(ProfileRequest::Set(profile)),
        Some(raw) => parse_holder(raw.as_str())?,
    };
    match profile {
        VideoProfile::Thumbnail => Ok(ProfileRequest::Release { holder }),
        VideoProfile::Hero => {
            let lease_s = parse_lease_s(body.get("lease_s"))?;
            Ok(ProfileRequest::Lease { holder, lease_s })
        }
    }
}

/// `lease_s`: absent or `null` is the default, else an integer in range.
fn parse_lease_s(raw: Option<&Value>) -> Result<u64, String> {
    match raw {
        None | Some(Value::Null) => Ok(LEASE_DEFAULT_S),
        Some(v) => match v.as_u64() {
            Some(n) if lease_seconds_in_range(n) => Ok(n),
            _ => Err("lease_s must be an integer from 5 to 120".to_string()),
        },
    }
}

/// Validate a `DELETE` query: the `holder` to release. The error is the `400`
/// detail.
pub(crate) fn parse_release_query(query: &HashMap<String, String>) -> Result<String, String> {
    parse_holder(query.get("holder").map(String::as_str))
}

fn parse_holder(raw: Option<&str>) -> Result<String, String> {
    let holder = raw.unwrap_or("");
    check_lease_holder(holder).map_err(str::to_string)?;
    Ok(holder.to_string())
}

/// `200` with the applied state, or `503` when the video service did not answer.
fn respond<E: Display>(result: Result<EncoderState, E>, unavailable: &str) -> Response {
    match result {
        Ok(applied) => (StatusCode::OK, Json(applied_body(&applied))).into_response(),
        Err(e) => detail(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("{unavailable}: {e}"),
        ),
    }
}

fn profile_mismatch() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})),
    )
        .into_response()
}

/// The applied encoder state, as the route reports it.
pub(crate) fn applied_body(applied: &EncoderState) -> Value {
    json!({
        // The effective profile: hero while any lease is live, else the base.
        "profile": applied.profile.as_str(),
        // What the drone runs with no lease live. `null` only when the video
        // service predates leases.
        "base_profile": applied.base_profile.map(VideoProfile::as_str),
        "width": applied.width,
        "height": applied.height,
        "fps": applied.fps,
        "bitrate_kbps": applied.bitrate_kbps,
        // The adaptive ladder's clamp, surfaced so an operator seeing 1200 kbps
        // on a hero can tell "the link is degraded" from "the wrong profile is
        // live". `null` when unclamped.
        "ceiling_kbps": applied.ceiling_kbps,
        // True when that clamp has been ACCEPTED but deliberately not pushed to
        // the encoder, because the change was too small to be worth the respawn
        // it would cost. `bitrate_kbps` above is then the rate the encoder is
        // really running, not the ceiling — reporting the ceiling as live would
        // be reporting a value the encoder never received.
        "ceiling_deferred": applied.ceiling_deferred,
        // Encoder-only respawns since the video service started, so a respawn
        // storm from an oscillating link is visible on this surface.
        "encoder_respawns": applied.encoder_respawns,
        // The live hero leases; `expires_in_s` is `null` for the vision tap's
        // lease, which has no expiry.
        "leases": applied.leases,
    })
}

fn is_ground_station(state: &AppState) -> bool {
    let cfg = crate::config::PairingConfig::load_from(&state.pairing_paths.config);
    let (profile, _role) = crate::profile::current_profile_and_role(&cfg.agent.profile);
    profile == "ground-station"
}

#[cfg(test)]
#[path = "video_profile_tests.rs"]
mod tests;
