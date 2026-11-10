//! The vehicle's MAVLink cameras: list them, take a photo, zoom.
//!
//! A camera that speaks the MAVLink camera protocol (a camera component on the
//! vehicle, or the autopilot's own camera backend) answers
//! `MAV_CMD_REQUEST_MESSAGE(CAMERA_INFORMATION)` with a `CAMERA_INFORMATION`
//! whose `flags` say what it can do. A surface reads that list first and drives
//! Photo and Zoom through the camera when one is advertised; without one it
//! falls back to a local frame grab and digital zoom.
//!
//! - `GET /api/v1/camera/mavlink` → `{"cameras": [...]}`. Each request sends
//!   `MAV_CMD_REQUEST_MESSAGE(259)` to every component of the flight
//!   controller's system and collects the `CAMERA_INFORMATION` answers for a
//!   short window. A camera that answered within the last minute stays listed,
//!   so one lost reply does not make it flicker. No flight controller, an MSP
//!   flight controller, no MAVLink socket or no answer is an empty list, never
//!   an error.
//! - `POST /api/v1/camera/mavlink/capture {"component_id"}` sends
//!   `MAV_CMD_IMAGE_START_CAPTURE` (one image: interval 0, count 1) to that
//!   component.
//! - `POST /api/v1/camera/mavlink/zoom {"component_id", "type", "value"}` sends
//!   `MAV_CMD_SET_CAMERA_ZOOM`: `type` `step` (value -1..1, -1 wide, 1 tele),
//!   `continuous` (rate -1..1, 0 stops) or `range` (0..100 percent of the
//!   camera's range).
//!
//! Both writes answer 200 `{"ok", "result", "component_id"}` once the command
//! was written: `result` is the camera's `MAV_RESULT` name (`ACCEPTED`,
//! `TEMPORARILY_REJECTED`, `DENIED`, `UNSUPPORTED`, `FAILED`, `IN_PROGRESS`,
//! `CANCELLED`) or `TIMEOUT` when no `COMMAND_ACK` arrived; `ok` is true for
//! `ACCEPTED` and `IN_PROGRESS`; a `statustext` the vehicle sent beside a
//! refusal is attached. The ACK is correlated and the send retried exactly as
//! `POST /api/command` does. A component that has not answered
//! `CAMERA_INFORMATION` (the write probes once more before refusing) is a 404;
//! a camera whose flags lack the capability is a 409; a bad zoom type or value
//! or component 0 is a 400; an MSP flight controller a 501; no flight
//! controller or no MAVLink socket a 503.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use ados_protocol::mavlink::ardupilotmega::{
    CameraCapFlags, MavCmd, MavMessage, CAMERA_INFORMATION_DATA, COMMAND_LONG_DATA,
};
use ados_protocol::mavlink::{self, MavHeader};

use crate::ipc::mavlink_client::SendError;
use crate::ipc::{FrameRead, MavlinkIpcClient};
use crate::routes::command::ack::{mav_result_name, send_awaiting_ack_from};
use crate::routes::command::{
    AckConfig, AckOutcome, MAV_RESULT_ACCEPTED, MAV_RESULT_IN_PROGRESS, SOURCE_COMPONENT_ID,
    SOURCE_SYSTEM_ID,
};
use crate::routes::detail;
use crate::state::AppState;

/// The `CAMERA_INFORMATION` message id `MAV_CMD_REQUEST_MESSAGE` asks for.
const MSG_ID_CAMERA_INFORMATION: f32 = 259.0;
/// `CAMERA_ZOOM_TYPE` values (common.xml).
const ZOOM_TYPE_STEP: f32 = 0.0;
const ZOOM_TYPE_CONTINUOUS: f32 = 1.0;
const ZOOM_TYPE_RANGE: f32 = 2.0;
/// A camera that answered within this long stays listed and addressable.
const SEEN_TTL: Duration = Duration::from_secs(60);

/// How long the routes wait on the vehicle. Injectable so a test drives short
/// bounds; the daemon uses [`CameraTiming::default`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct CameraTiming {
    /// How long a probe collects `CAMERA_INFORMATION` answers.
    pub(crate) probe_window: Duration,
    /// The `COMMAND_ACK` wait and retry policy of a capture or zoom.
    pub(crate) ack: AckConfig,
}

impl Default for CameraTiming {
    fn default() -> Self {
        Self {
            probe_window: Duration::from_millis(1000),
            ack: AckConfig::default(),
        }
    }
}

/// One advertised camera, as the list reports it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MavlinkCamera {
    /// The MAVLink component the camera answers from; the address a capture or
    /// zoom names.
    pub component_id: u8,
    pub vendor: String,
    pub model: String,
    /// `firmware_version` as sent: `(dev << 24) | (patch << 16) | (minor << 8) | major`.
    pub firmware_version: u32,
    /// The raw `CAMERA_CAP_FLAGS` bitmask.
    pub flags: u32,
    /// `CAMERA_CAP_FLAGS_CAPTURE_IMAGE`: a capture is accepted.
    pub can_capture_image: bool,
    /// `CAMERA_CAP_FLAGS_CAPTURE_VIDEO`.
    pub can_capture_video: bool,
    /// `CAMERA_CAP_FLAGS_HAS_BASIC_ZOOM`: a zoom is accepted.
    pub can_zoom: bool,
    /// `CAMERA_CAP_FLAGS_HAS_BASIC_FOCUS`.
    pub can_focus: bool,
    /// `CAMERA_CAP_FLAGS_HAS_VIDEO_STREAM`.
    pub has_video_stream: bool,
    pub resolution_h: u16,
    pub resolution_v: u16,
    /// Focal length in millimetres; `null` when the camera does not say.
    pub focal_length_mm: Option<f32>,
    pub lens_id: u8,
    /// The camera definition file URI; empty when the camera has none.
    pub cam_definition_uri: String,
}

impl MavlinkCamera {
    fn from_info(component_id: u8, info: &CAMERA_INFORMATION_DATA) -> Self {
        let flags = info.flags;
        let has = |f: CameraCapFlags| flags.contains(f);
        Self {
            component_id,
            vendor: text(&info.vendor_name),
            model: text(&info.model_name),
            firmware_version: info.firmware_version,
            flags: flags.bits(),
            can_capture_image: has(CameraCapFlags::CAMERA_CAP_FLAGS_CAPTURE_IMAGE),
            can_capture_video: has(CameraCapFlags::CAMERA_CAP_FLAGS_CAPTURE_VIDEO),
            can_zoom: has(CameraCapFlags::CAMERA_CAP_FLAGS_HAS_BASIC_ZOOM),
            can_focus: has(CameraCapFlags::CAMERA_CAP_FLAGS_HAS_BASIC_FOCUS),
            has_video_stream: has(CameraCapFlags::CAMERA_CAP_FLAGS_HAS_VIDEO_STREAM),
            resolution_h: info.resolution_h,
            resolution_v: info.resolution_v,
            focal_length_mm: (info.focal_length.is_finite() && info.focal_length > 0.0)
                .then_some(info.focal_length),
            lens_id: info.lens_id,
            cam_definition_uri: text(&info.cam_definition_uri[..]),
        }
    }
}

/// A NUL-padded MAVLink char array as a trimmed string.
fn text(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

/// Each answering camera and when it last answered, by `(system, component)`.
type Seen = BTreeMap<(u8, u8), (MavlinkCamera, Instant)>;

/// The cameras that answered, keyed by `(system, component)` so a vehicle
/// that changes its system id never inherits another's cameras.
#[derive(Clone, Default)]
pub struct MavlinkCameras {
    seen: Arc<parking_lot::Mutex<Seen>>,
    timing: CameraTiming,
}

impl MavlinkCameras {
    /// The same registry with other wait bounds (tests).
    #[cfg(test)]
    pub(crate) fn with_timing(timing: CameraTiming) -> Self {
        Self {
            seen: Arc::default(),
            timing,
        }
    }

    fn record(&self, system: u8, found: Vec<MavlinkCamera>) {
        let now = Instant::now();
        let mut seen = self.seen.lock();
        for cam in found {
            seen.insert((system, cam.component_id), (cam, now));
        }
        seen.retain(|_, (_, at)| now.duration_since(*at) <= SEEN_TTL);
    }

    /// The live cameras of `system`, by component id.
    fn current(&self, system: u8) -> Vec<MavlinkCamera> {
        let now = Instant::now();
        self.seen
            .lock()
            .range((system, 0)..=(system, u8::MAX))
            .filter(|(_, (_, at))| now.duration_since(*at) <= SEEN_TTL)
            .map(|(_, (cam, _))| cam.clone())
            .collect()
    }

    fn get(&self, system: u8, component: u8) -> Option<MavlinkCamera> {
        self.current(system)
            .into_iter()
            .find(|c| c.component_id == component)
    }
}

/// The `MAV_CMD_REQUEST_MESSAGE(CAMERA_INFORMATION)` frame to every component
/// of `system`.
fn request_camera_information(system: u8) -> COMMAND_LONG_DATA {
    COMMAND_LONG_DATA {
        target_system: system,
        target_component: 0,
        command: MavCmd::MAV_CMD_REQUEST_MESSAGE,
        confirmation: 0,
        param1: MSG_ID_CAMERA_INFORMATION,
        param2: 0.0,
        param3: 0.0,
        param4: 0.0,
        param5: 0.0,
        param6: 0.0,
        param7: 0.0,
    }
}

/// Ask every component of `system` for its `CAMERA_INFORMATION` and collect the
/// answers for `window`. An unreachable MAVLink socket is the `Err`; a vehicle
/// that does not answer is an empty list.
async fn probe(
    client: &MavlinkIpcClient,
    system: u8,
    window: Duration,
) -> Result<Vec<MavlinkCamera>, SendError> {
    let mut stream = client.open_ack_stream().await?;
    let header = MavHeader {
        system_id: SOURCE_SYSTEM_ID,
        component_id: SOURCE_COMPONENT_ID,
        sequence: 0,
    };
    let frame = mavlink::serialize_v2(
        header,
        &MavMessage::COMMAND_LONG(request_camera_information(system)),
    )
    .map_err(|e| SendError::Io(std::io::Error::other(e.to_string())))?;
    stream.write_frame(&frame).await?;

    let mut found: BTreeMap<u8, MavlinkCamera> = BTreeMap::new();
    let deadline = Instant::now() + window;
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        match stream.read_frame(deadline - now).await {
            FrameRead::Frame(payload) => {
                if let Ok((h, MavMessage::CAMERA_INFORMATION(info))) = mavlink::parse_any(&payload)
                {
                    if h.system_id == system {
                        found.insert(
                            h.component_id,
                            MavlinkCamera::from_info(h.component_id, &info),
                        );
                    }
                }
            }
            FrameRead::Timeout | FrameRead::Eof => break,
        }
    }
    Ok(found.into_values().collect())
}

/// `GET /api/v1/camera/mavlink` → `{"cameras": [MavlinkCamera, ...]}`, always 200.
pub async fn list_cameras(State(state): State<AppState>) -> Json<Value> {
    if !state.fc_connected() || state.fc_speaks_msp() {
        return Json(json!({ "cameras": [] }));
    }
    let system = crate::fc_target::fc_target(&state).0;
    let registry = &state.mavlink_cameras;
    match probe(&state.mavlink, system, registry.timing.probe_window).await {
        Ok(found) => registry.record(system, found),
        Err(e) => tracing::debug!(error = %e, "camera information probe not sent"),
    }
    Json(json!({ "cameras": registry.current(system) }))
}

/// What a write needs the camera to advertise.
#[derive(Debug, Clone, Copy)]
enum Needs {
    Capture,
    Zoom,
}

/// Why a write is refused before any send: the status and the `detail`.
type Refusal = (StatusCode, String);

/// Gate a write and resolve its camera: the flight controller, the component,
/// and the capability. Returns the vehicle system id, or the refusal.
async fn resolve(state: &AppState, component: u8, needs: Needs) -> Result<u8, Refusal> {
    let refuse = |status: StatusCode, why: &str| Err((status, why.to_string()));
    if !state.fc_connected() {
        return refuse(StatusCode::SERVICE_UNAVAILABLE, "FC not connected");
    }
    if state.fc_speaks_msp() {
        return refuse(
            StatusCode::NOT_IMPLEMENTED,
            "this flight controller speaks MSP (Betaflight/iNav); MAVLink \
             camera commands do not apply to it",
        );
    }
    if component == 0 {
        return refuse(
            StatusCode::BAD_REQUEST,
            "component_id must name one camera (1-255)",
        );
    }
    let system = crate::fc_target::fc_target(state).0;
    let registry = &state.mavlink_cameras;
    let camera = match registry.get(system, component) {
        Some(cam) => cam,
        None => {
            match probe(&state.mavlink, system, registry.timing.probe_window).await {
                Ok(found) => registry.record(system, found),
                Err(e) => {
                    tracing::warn!(error = %e, "camera information probe not sent");
                    return refuse(StatusCode::SERVICE_UNAVAILABLE, "No MAVLink connection");
                }
            }
            match registry.get(system, component) {
                Some(cam) => cam,
                None => {
                    return refuse(
                        StatusCode::NOT_FOUND,
                        &format!("no MAVLink camera answers at component {component}"),
                    )
                }
            }
        }
    };
    let (capable, what) = match needs {
        Needs::Capture => (camera.can_capture_image, "image capture"),
        Needs::Zoom => (camera.can_zoom, "zoom"),
    };
    if !capable {
        return refuse(
            StatusCode::CONFLICT,
            &format!("the camera at component {component} does not advertise {what}"),
        );
    }
    Ok(system)
}

/// Send `long` and answer `{"ok", "result", "component_id", "statustext"?}`.
/// The ACK must come from the addressed camera component, so concurrent writes
/// to two cameras of one vehicle never take each other's answer. `resend`
/// false sends once: a resent capture whose ACK was lost shoots twice.
async fn send(state: &AppState, long: COMMAND_LONG_DATA, resend: bool) -> Response {
    let component = long.target_component;
    let mut ack = state.mavlink_cameras.timing.ack;
    if !resend {
        ack.retries = 0;
    }
    match send_awaiting_ack_from(&state.mavlink, &long, &ack, Some(component)).await {
        Ok(outcome) => {
            let mut body = json!({ "component_id": component });
            match outcome {
                AckOutcome::Acked { result, statustext } => {
                    body["ok"] =
                        json!(result == MAV_RESULT_ACCEPTED || result == MAV_RESULT_IN_PROGRESS);
                    body["result"] = json!(mav_result_name(result));
                    if let Some(txt) = statustext {
                        body["statustext"] = json!(txt);
                    }
                }
                AckOutcome::NoAck => {
                    body["ok"] = json!(false);
                    body["result"] = json!("TIMEOUT");
                }
            }
            (StatusCode::OK, Json(body)).into_response()
        }
        Err(e) => {
            tracing::warn!(error = %e, command = ?long.command, "camera command not sent");
            detail(StatusCode::SERVICE_UNAVAILABLE, "No MAVLink connection")
        }
    }
}

/// A `COMMAND_LONG` to one camera component.
fn camera_command(target: (u8, u8), command: MavCmd, params: [f32; 7]) -> COMMAND_LONG_DATA {
    let [param1, param2, param3, param4, param5, param6, param7] = params;
    COMMAND_LONG_DATA {
        target_system: target.0,
        target_component: target.1,
        command,
        confirmation: 0,
        param1,
        param2,
        param3,
        param4,
        param5,
        param6,
        param7,
    }
}

/// The `POST /api/v1/camera/mavlink/capture` body.
#[derive(Debug, Deserialize)]
pub struct CaptureRequest {
    pub component_id: u8,
}

/// `MAV_CMD_IMAGE_START_CAPTURE` for one image: every camera of the addressed
/// component (id 0), interval 0, count 1, no sequence number.
fn capture_command(target: (u8, u8)) -> COMMAND_LONG_DATA {
    camera_command(
        target,
        MavCmd::MAV_CMD_IMAGE_START_CAPTURE,
        [0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0],
    )
}

/// `POST /api/v1/camera/mavlink/capture`: one still image on the camera.
pub async fn post_capture(
    State(state): State<AppState>,
    Json(req): Json<CaptureRequest>,
) -> Response {
    let system = match resolve(&state, req.component_id, Needs::Capture).await {
        Ok(system) => system,
        Err((status, why)) => return detail(status, why),
    };
    send(&state, capture_command((system, req.component_id)), false).await
}

/// How a zoom `value` is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ZoomType {
    /// One step: -1 wide, 1 tele.
    Step,
    /// A zoom rate until stopped: -1..1, 0 stops.
    Continuous,
    /// A position: 0..100 percent of the camera's zoom range.
    Range,
}

/// The `POST /api/v1/camera/mavlink/zoom` body.
#[derive(Debug, Deserialize)]
pub struct ZoomRequest {
    pub component_id: u8,
    #[serde(rename = "type")]
    pub kind: ZoomType,
    pub value: f32,
}

/// The `(type, value)` params of a zoom, or why the value is refused.
fn zoom_params(kind: ZoomType, value: f32) -> Result<(f32, f32), String> {
    let (zoom_type, lo, hi) = match kind {
        ZoomType::Step => (ZOOM_TYPE_STEP, -1.0, 1.0),
        ZoomType::Continuous => (ZOOM_TYPE_CONTINUOUS, -1.0, 1.0),
        ZoomType::Range => (ZOOM_TYPE_RANGE, 0.0, 100.0),
    };
    if !value.is_finite() || value < lo || value > hi {
        return Err(format!("zoom value must be within {lo}..{hi}"));
    }
    Ok((zoom_type, value))
}

/// `MAV_CMD_SET_CAMERA_ZOOM`: the type, the value, every camera of the
/// addressed component (id 0).
fn zoom_command(target: (u8, u8), zoom_type: f32, value: f32) -> COMMAND_LONG_DATA {
    camera_command(
        target,
        MavCmd::MAV_CMD_SET_CAMERA_ZOOM,
        [zoom_type, value, 0.0, 0.0, 0.0, 0.0, 0.0],
    )
}

/// `POST /api/v1/camera/mavlink/zoom`: zoom the camera.
pub async fn post_zoom(State(state): State<AppState>, Json(req): Json<ZoomRequest>) -> Response {
    let (zoom_type, value) = match zoom_params(req.kind, req.value) {
        Ok(p) => p,
        Err(why) => return detail(StatusCode::BAD_REQUEST, why),
    };
    let system = match resolve(&state, req.component_id, Needs::Zoom).await {
        Ok(system) => system,
        Err((status, why)) => return detail(status, why),
    };
    send(
        &state,
        zoom_command((system, req.component_id), zoom_type, value),
        true,
    )
    .await
}

#[cfg(test)]
#[path = "camera_mavlink_tests.rs"]
mod tests;
