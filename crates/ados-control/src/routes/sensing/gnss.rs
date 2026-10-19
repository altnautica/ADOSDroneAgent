//! `GET /api/v1/gnss`, `PUT /api/v1/gnss/base` and `POST /api/v1/gnss/rtcm`.
//!
//! The read serves the `ados-gnss` sidecar. The base write is applied live by
//! `ados-gnss` over its command socket. The RTCM write publishes the bytes on
//! the sensor bus, where the MAVLink router injects them to the flight
//! controller as `GPS_RTCM_DATA` (and a sensor hub forwards them to its
//! receiver); the reply counts the `GPS_RTCM_DATA` fragments that makes.

use std::path::Path;
use std::time::{Duration, SystemTime};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use ados_sensorbus::{rtcm, BusFrame, Kind, Origin};

use super::{fresh_map, with_present, SensingPaths, FRESH};
use crate::state::AppState;

/// How long the GNSS service has to answer a base command.
const CMD_TIMEOUT: Duration = Duration::from_secs(3);

pub async fn get_gnss(State(state): State<AppState>) -> Json<Value> {
    Json(gnss_body(
        &SensingPaths::from_state(&state),
        SystemTime::now(),
    ))
}

pub(crate) fn gnss_body(paths: &SensingPaths, now: SystemTime) -> Value {
    match fresh_map(&paths.sidecar("gnss.json"), Some(FRESH), now) {
        Some(m) => with_present(true, m),
        None => {
            let mut rest = Map::new();
            rest.insert("receivers".into(), json!([]));
            rest.insert("base".into(), Value::Null);
            rest.insert("heading".into(), Value::Null);
            with_present(false, rest)
        }
    }
}

fn err(status: StatusCode, code: &str, detail: impl Into<String>) -> Response {
    (
        status,
        Json(json!({"error": code, "detail": detail.into()})),
    )
        .into_response()
}

pub async fn put_base(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    put_base_at(
        &SensingPaths::from_state(&state).sidecar("gnss-cmd.sock"),
        body,
    )
    .await
}

pub(crate) async fn put_base_at(sock: &Path, body: Value) -> Response {
    if !matches!(
        body.get("mode").and_then(Value::as_str),
        Some("off" | "survey" | "fixed")
    ) {
        return err(
            StatusCode::BAD_REQUEST,
            "E_GNSS_BAD_REQUEST",
            "mode must be off, survey or fixed",
        );
    }
    let reply = match tokio::time::timeout(CMD_TIMEOUT, command(sock, &body)).await {
        Ok(Ok(v)) => v,
        _ => {
            return err(
                StatusCode::SERVICE_UNAVAILABLE,
                "E_GNSS_UNAVAILABLE",
                "the GNSS service is not running",
            )
        }
    };
    if reply["ok"] == true {
        return Json(reply["base"].clone()).into_response();
    }
    let code = reply["error"].as_str().unwrap_or("E_GNSS_BAD_REQUEST");
    let status = match code {
        "E_GNSS_NO_RECEIVER" | "E_GNSS_UNSUPPORTED" => StatusCode::CONFLICT,
        _ => StatusCode::BAD_REQUEST,
    };
    err(status, code, reply["detail"].as_str().unwrap_or_default())
}

async fn command(sock: &Path, base: &Value) -> std::io::Result<Value> {
    let mut stream = UnixStream::connect(sock).await?;
    let mut line = serde_json::to_vec(&json!({"cmd": "set_base", "base": base}))?;
    line.push(b'\n');
    stream.write_all(&line).await?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).await?;
    Ok(serde_json::from_str(&reply)?)
}

pub async fn post_rtcm(State(state): State<AppState>, body: axum::body::Bytes) -> Response {
    let sock = SensingPaths::from_state(&state).sidecar("sensors.sock");
    post_rtcm_at(&sock, &body).await
}

pub(crate) async fn post_rtcm_at(sock: &Path, body: &[u8]) -> Response {
    if body.is_empty() {
        return err(StatusCode::BAD_REQUEST, "E_RTCM_EMPTY", "empty body");
    }
    let fragments = match rtcm::fragment_count(body) {
        Ok(n) => n,
        Err(e) => return err(StatusCode::BAD_REQUEST, "E_RTCM_TOO_LONG", e.to_string()),
    };
    let frame = BusFrame::new(Kind::Rtcm, "rest", Origin::Host, body.to_vec());
    if let Err(e) = ados_sensorbus::publish_once(sock, &frame).await {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "E_SENSOR_BUS_DOWN",
            format!("the sensor bus is not reachable: {e}"),
        );
    }
    Json(json!({"ok": true, "fragments": fragments})).into_response()
}

#[cfg(test)]
#[path = "gnss_tests.rs"]
mod tests;
