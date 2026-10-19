//! The `camera.control.set` plugin method: set exposure, gain or the
//! auto-exposure mode of a camera the vision engine owns, and report the
//! values the driver reads back.
//!
//! The plugin host relays the request to the vision engine over
//! `vision.sock` under the same method name. The engine applies it through
//! the device handle it already streams from (the device is never opened a
//! second time), reads every touched control back and answers with a
//! [`CameraControlResult`]. `applied` is true only when every requested
//! control was set and its read-back matches (exposure and gain within one
//! driver step); otherwise `reason` carries one of the [`reason`] codes.
//!
//! Only cameras configured `capture: true` in `cameras[]` may be controlled;
//! any other camera is refused with [`reason::NOT_CAPTURE_CAMERA`].
//!
//! `gain` is a multiplier (`1.0` is unity gain), on the request and on the
//! read-back alike. The engine maps it to the driver's gain code linearly,
//! taking the control's default code as 1×; a control whose default does not
//! name a unit gain is refused with [`reason::GAIN_UNIT_UNKNOWN`] rather than
//! guessed at.
//!
//! The types are the wire shape on both hops: the SDK encodes the request,
//! the host and the engine decode it, and the reply travels back unchanged.

use rmpv::Value;
use serde::{Deserialize, Serialize};

/// The plugin method, also the `vision.sock` method the host relays it as.
pub const CAMERA_CONTROL_METHOD: &str = "camera.control.set";
/// The capability the method is gated on.
pub const CAMERA_CONTROL_CAPABILITY: &str = "camera.control.write";
/// How long the engine waits, per request, for the camera's capture thread to
/// apply it. A request not applied by then is answered
/// [`reason::NOT_STREAMING`] and dropped, never applied later.
pub const ENGINE_WAIT_MS: u64 = 1_500;

/// Why a request was not applied.
pub mod reason {
    /// The capture camera is not a local V4L2 device (a simulated RTSP rig):
    /// it has no controls this node can set.
    pub const NOT_V4L2: &str = "not_v4l2";
    /// The driver lacks a requested control, or reports it read-only.
    pub const CONTROL_UNSUPPORTED: &str = "control_unsupported";
    /// No camera with that id runs on this node.
    pub const UNKNOWN_CAMERA: &str = "unknown_camera";
    /// The camera is not configured `capture: true`, so a plugin may not
    /// change its controls (the video pipeline's own camera, for one).
    pub const NOT_CAPTURE_CAMERA: &str = "not_capture_camera";
    /// The driver's gain control has no known unit gain, so a multiplier
    /// cannot be mapped to it.
    pub const GAIN_UNIT_UNKNOWN: &str = "gain_unit_unknown";
    /// The camera's capture is not running, so there is no open device to
    /// set the controls on.
    pub const NOT_STREAMING: &str = "not_streaming";
    /// A requested value lies outside the driver's range.
    pub const OUT_OF_RANGE: &str = "out_of_range";
    /// The driver refused to set a control.
    pub const SET_FAILED: &str = "set_failed";
    /// The driver accepted the values but reads back something else.
    pub const READBACK_MISMATCH: &str = "readback_mismatch";
    /// A manual exposure time was asked for together with auto exposure.
    pub const CONFLICTING_REQUEST: &str = "conflicting_request";
    /// A gain that is negative or not a number.
    pub const INVALID_VALUE: &str = "invalid_value";
    /// Nothing to set; the reply still carries the current read-back.
    pub const EMPTY_REQUEST: &str = "empty_request";
    /// The camera's control queue is full.
    pub const BUSY: &str = "busy";
}

/// `camera.control.set` arguments. An absent field leaves that control as it
/// is. `exposure_us` implies manual exposure; `gain` is a multiplier (1.0 is
/// unity gain).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CameraControlRequest {
    pub camera_id: String,
    #[serde(default)]
    pub exposure_us: Option<u32>,
    #[serde(default)]
    pub gain: Option<f32>,
    #[serde(default)]
    pub auto_exposure: Option<bool>,
}

/// `camera.control.set` reply. `exposure_us` and `gain` (a multiplier) are the
/// values the driver reads back (`None` when the camera has no such control,
/// its gain unit is unknown, or it was not reached), not the requested ones.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CameraControlResult {
    pub applied: bool,
    #[serde(default)]
    pub exposure_us: Option<u32>,
    #[serde(default)]
    pub gain: Option<f32>,
    #[serde(default)]
    pub reason: Option<String>,
}

impl CameraControlResult {
    /// Not applied, for `reason`, with nothing read back.
    pub fn refused(reason: &str) -> Self {
        Self {
            applied: false,
            exposure_us: None,
            gain: None,
            reason: Some(reason.to_string()),
        }
    }
}

/// Decode a msgpack map into `T`; `what` prefixes the error.
fn decode<T: serde::de::DeserializeOwned>(value: &Value, what: &str) -> Result<T, String> {
    rmpv::ext::from_value(value.clone()).map_err(|e| format!("{what}: {e}"))
}

fn map(entries: Vec<(&str, Option<Value>)>) -> Value {
    Value::Map(
        entries
            .into_iter()
            .filter_map(|(k, v)| v.map(|v| (Value::from(k), v)))
            .collect(),
    )
}

impl CameraControlRequest {
    /// The request as a msgpack map; absent fields are omitted.
    pub fn to_args(&self) -> Value {
        map(vec![
            ("camera_id", Some(Value::from(self.camera_id.as_str()))),
            ("exposure_us", self.exposure_us.map(Value::from)),
            ("gain", self.gain.map(Value::from)),
            ("auto_exposure", self.auto_exposure.map(Value::from)),
        ])
    }

    /// Decode request arguments.
    pub fn from_args(args: &Value) -> Result<Self, String> {
        decode(args, "camera.control.set arguments are invalid")
    }
}

impl CameraControlResult {
    /// The reply as a msgpack map; absent fields are omitted.
    pub fn to_value(&self) -> Value {
        map(vec![
            ("applied", Some(Value::from(self.applied))),
            ("exposure_us", self.exposure_us.map(Value::from)),
            ("gain", self.gain.map(Value::from)),
            ("reason", self.reason.as_deref().map(Value::from)),
        ])
    }

    /// Decode a reply.
    pub fn from_value(value: &Value) -> Result<Self, String> {
        decode(value, "camera.control.set reply did not decode")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_round_trips_and_omits_absent_fields() {
        let req = CameraControlRequest {
            camera_id: "cam_fl".into(),
            exposure_us: Some(2_000),
            gain: Some(4.0),
            auto_exposure: None,
        };
        let args = req.to_args();
        let Value::Map(entries) = &args else {
            panic!("a map")
        };
        assert!(!entries
            .iter()
            .any(|(k, _)| k.as_str() == Some("auto_exposure")));
        assert_eq!(CameraControlRequest::from_args(&args).unwrap(), req);
    }

    #[test]
    fn a_request_without_optional_fields_decodes() {
        let args = Value::Map(vec![(Value::from("camera_id"), Value::from("c"))]);
        let req = CameraControlRequest::from_args(&args).unwrap();
        assert_eq!(req.camera_id, "c");
        assert_eq!(
            (req.exposure_us, req.gain, req.auto_exposure),
            (None, None, None)
        );
        // An integer gain, as a dynamically typed client may send, decodes.
        let args = Value::Map(vec![
            (Value::from("camera_id"), Value::from("c")),
            (Value::from("gain"), Value::from(2)),
        ]);
        assert_eq!(
            CameraControlRequest::from_args(&args).unwrap().gain,
            Some(2.0)
        );
        assert!(CameraControlRequest::from_args(&Value::Map(vec![])).is_err());
    }

    #[test]
    fn a_result_round_trips() {
        let applied = CameraControlResult {
            applied: true,
            exposure_us: Some(2_000),
            gain: Some(4.0),
            reason: None,
        };
        assert_eq!(
            CameraControlResult::from_value(&applied.to_value()).unwrap(),
            applied
        );
        let refused = CameraControlResult::refused(reason::NOT_V4L2);
        assert_eq!(
            CameraControlResult::from_value(&refused.to_value()).unwrap(),
            refused
        );
    }
}
