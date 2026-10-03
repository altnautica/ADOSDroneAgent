//! Ground-station WFB radio-config write route.
//!
//! `PUT /api/v1/ground-station/wfb` stores the ground station's radio channel
//! under `video.wfb.channel`. The read view lives in
//! [`crate::routes::gs_status::get_wfb`] and reports the channel the radio is
//! actually tuned to; this write only changes the stored value, which the
//! receive services read on their own cadence.
//!
//! The body is `{"channel": <int>}`. The channel must be one of the standard
//! WFB channels ([`ados_protocol::wfb_status::STANDARD_CHANNELS`]); anything
//! else is a 400 before the file is touched. Any other field is refused with a
//! 400 rather than stored: nothing reads a stored `fec` or `bitrate_profile`
//! from here (the link FEC and MCS are set through `/api/video/config`), so
//! accepting them would report success for a setting with no effect. The
//! armed-interlock override field `force` is accepted and ignored here (the
//! front's interlock reads it).
//!
//! The merge goes through the shared config store, which preserves every other
//! key and the file's 0600 mode. The route has no other effect, so a failed
//! write is a 500 `{channel, persisted: false, persist_error}` and a clean one
//! is `{channel, persisted: true}`.
//!
//! Like every ground-station route, this first gates on the resolved profile
//! being a ground station and returns
//! `404 {"detail":{"error":{"code":"E_PROFILE_MISMATCH"}}}` on a drone.

use std::path::{Path, PathBuf};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};

use crate::config_store::{section_path, update_config};
use crate::state::AppState;

/// The `E_PROFILE_MISMATCH` 404 every ground-station route answers on a drone.
fn profile_mismatch() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})),
    )
        .into_response()
}

/// True when the resolved profile is a ground station.
fn is_ground_station() -> bool {
    let cfg = crate::config::PairingConfig::load();
    let (profile, _role) = crate::profile::current_profile_and_role(&cfg.agent.profile);
    profile == "ground-station"
}

/// The agent config path (`ADOS_CONFIG`, default `/etc/ados/config.yaml`).
fn config_yaml_path() -> PathBuf {
    PathBuf::from(
        std::env::var("ADOS_CONFIG").unwrap_or_else(|_| crate::config::CONFIG_YAML.to_string()),
    )
}

/// A 400 with the `{detail:{error:{code,message}}}` shape the ground-station
/// routes use.
fn bad_request(code: &str, message: impl Into<String>) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"detail": {"error": {"code": code, "message": message.into()}}})),
    )
        .into_response()
}

/// `PUT /api/v1/ground-station/wfb` → `{channel, persisted[, persist_error]}`.
pub async fn put_ground_station_wfb(
    State(_state): State<AppState>,
    Json(body): Json<Map<String, Value>>,
) -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }
    put_wfb_at(&config_yaml_path(), &body)
}

/// Validate the body and persist the channel, against an explicit config path.
fn put_wfb_at(config_path: &Path, body: &Map<String, Value>) -> Response {
    let unsupported: Vec<&str> = body
        .keys()
        .map(String::as_str)
        .filter(|k| !matches!(*k, "channel" | "force"))
        .collect();
    if !unsupported.is_empty() {
        return bad_request(
            "E_UNSUPPORTED_FIELD",
            format!(
                "Only `channel` can be set here; not stored: {}",
                unsupported.join(", ")
            ),
        );
    }
    let channel = match body.get("channel") {
        Some(v) => match v.as_i64() {
            Some(ch) if ados_protocol::wfb_status::get_channel(ch).is_some() => ch,
            _ => {
                let allowed: Vec<String> = ados_protocol::wfb_status::STANDARD_CHANNELS
                    .iter()
                    .map(|c| c.channel_number.to_string())
                    .collect();
                return bad_request(
                    "E_INVALID_CHANNEL",
                    format!("channel must be one of {}", allowed.join(", ")),
                );
            }
        },
        None => return bad_request("E_INVALID_CHANNEL", "channel is required"),
    };

    let outcome = update_config(config_path, |root| {
        section_path(root, &["video", "wfb"]).insert(
            serde_norway::Value::String("channel".to_string()),
            serde_norway::Value::Number(channel.into()),
        );
        Ok(())
    });
    match outcome {
        Ok(_) => Json(json!({"channel": channel, "persisted": true})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "channel": channel,
                "persisted": false,
                "persist_error": e.to_string(),
            })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn body(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[tokio::test]
    async fn a_standard_channel_is_stored_and_the_rest_of_the_file_kept() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(
            &cfg,
            "agent:\n  name: gs-1\nvideo:\n  wfb:\n    channel: 149\n",
        )
        .unwrap();

        let resp = put_wfb_at(&cfg, &body(json!({"channel": 161, "force": true})));
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            body_json(resp).await,
            json!({"channel": 161, "persisted": true})
        );
        let parsed: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let wfb = parsed.get("video").and_then(|v| v.get("wfb")).unwrap();
        assert_eq!(wfb.get("channel").and_then(|c| c.as_i64()), Some(161));
        assert_eq!(
            parsed
                .get("agent")
                .and_then(|a| a.get("name"))
                .and_then(|n| n.as_str()),
            Some("gs-1")
        );
    }

    #[tokio::test]
    async fn an_off_table_channel_or_an_unconsumed_field_is_refused_unwritten() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(&cfg, "video:\n  wfb:\n    channel: 149\n").unwrap();

        for bad in [
            json!({"channel": 0}),
            json!({"channel": 14}),
            json!({"channel": "149"}),
            json!({}),
            json!({"channel": 157, "fec": "8/12"}),
            json!({"bitrate_profile": "long-range"}),
        ] {
            let resp = put_wfb_at(&cfg, &body(bad.clone()));
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{bad}");
        }
        assert_eq!(
            std::fs::read_to_string(&cfg).unwrap(),
            "video:\n  wfb:\n    channel: 149\n",
            "a refused write leaves the file alone"
        );
    }

    #[tokio::test]
    async fn a_write_fault_is_a_server_error() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let cfg = blocker.join("config.yaml");
        let resp = put_wfb_at(&cfg, &body(json!({"channel": 157})));
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let b = body_json(resp).await;
        assert_eq!(b["persisted"], json!(false));
        assert!(b["persist_error"].as_str().is_some());
    }
}
