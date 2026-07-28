//! Drone-side attention-profile write: **`POST /api/video/profile`**.
//!
//! Body `{"profile": "hero" | "thumbnail"}`; the response echoes the settings
//! the encoder ACTUALLY applied, not the ones that were asked for.
//!
//! The whole fleet shares one 20 MHz channel and one radio per node, so exactly
//! one drone streams full video (`hero`, 1280x720/30 fps/4000 kbps) and every
//! other registered drone streams 320x180/1 fps/50 kbps. The airtime arithmetic
//! that fixes those numbers, and the reason 24 drones do NOT fit at MCS 1, lives
//! on `ados_video::profile`.
//!
//! This route is a thin REST surface over `ados-video`'s encoder command socket
//! — the one entry point both this route and the adaptive-bitrate ladder in
//! `ados-radio` drive, so a hero switch and a link-quality clamp compose instead
//! of clobbering one another. `ados-video` owns the encoder; this front only
//! asks. A rig with no video pipeline (the socket is absent, or the service is
//! down) answers `503` rather than pretending the switch took.
//!
//! Normally this arrives over the radio's relay-proxy from the ground station's
//! fleet-hero route, but it is a first-class local route too: a bench operator
//! can promote a drone over LAN with no ground station in the path.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use ados_video::profile::{EncoderState, VideoProfile};

use crate::routes::detail;
use crate::state::AppState;

/// `POST /api/video/profile` — set this drone's video attention profile.
pub async fn post_video_profile(
    State(state): State<AppState>,
    body: Option<Json<Value>>,
) -> Response {
    // A ground station has no air-side encoder to retarget; the same
    // profile-mismatch shape every profile-gated route uses.
    if is_ground_station(&state) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})),
        )
            .into_response();
    }

    let body = body.map(|Json(v)| v).unwrap_or(Value::Null);
    let Some(profile) = body
        .get("profile")
        .and_then(Value::as_str)
        .and_then(VideoProfile::parse)
    else {
        return detail(
            StatusCode::BAD_REQUEST,
            "profile must be \"hero\" or \"thumbnail\"",
        );
    };

    match ados_video::profile::set_profile(profile).await {
        Ok(applied) => (StatusCode::OK, Json(applied_body(&applied))).into_response(),
        Err(e) => {
            tracing::warn!(error = %e, requested = %profile, "video_profile_set_failed");
            detail(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("video pipeline did not accept the profile change: {e}"),
            )
        }
    }
}

/// The applied encoder state, as the route reports it.
pub(crate) fn applied_body(applied: &EncoderState) -> Value {
    json!({
        "profile": applied.profile.as_str(),
        "width": applied.width,
        "height": applied.height,
        "fps": applied.fps,
        "bitrate_kbps": applied.bitrate_kbps,
        // The adaptive ladder's clamp, surfaced so an operator seeing 1200 kbps
        // on a hero can tell "the link is degraded" from "the wrong profile is
        // live". `null` when unclamped.
        "ceiling_kbps": applied.ceiling_kbps,
    })
}

fn is_ground_station(state: &AppState) -> bool {
    let cfg = crate::config::PairingConfig::load_from(&state.pairing_paths.config);
    let (profile, _role) = crate::profile::current_profile_and_role(&cfg.agent.profile);
    profile == "ground-station"
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_video::profile::EncoderSettings;

    #[test]
    fn the_response_reports_what_was_applied_including_an_active_clamp() {
        let clamped = EncoderState::new(
            VideoProfile::Hero,
            Some(1200),
            EncoderSettings {
                width: 1280,
                height: 720,
                fps: 30,
                bitrate_kbps: 1200,
            },
        );
        let body = applied_body(&clamped);
        assert_eq!(body["profile"], "hero");
        assert_eq!(body["width"], 1280);
        assert_eq!(body["fps"], 30);
        // The applied bitrate is the clamped one, not the profile's nominal
        // 4000 — reporting 4000 here would tell the operator the link is fine.
        assert_eq!(body["bitrate_kbps"], 1200);
        assert_eq!(body["ceiling_kbps"], 1200);
    }

    #[test]
    fn an_unclamped_thumbnail_reports_a_null_ceiling() {
        let st = EncoderState::new(
            VideoProfile::Thumbnail,
            None,
            EncoderSettings {
                width: 320,
                height: 180,
                fps: 1,
                bitrate_kbps: 50,
            },
        );
        let body = applied_body(&st);
        assert_eq!(body["profile"], "thumbnail");
        assert_eq!(body["bitrate_kbps"], 50);
        assert!(body["ceiling_kbps"].is_null());
    }
}
