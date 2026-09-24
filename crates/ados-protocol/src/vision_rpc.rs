//! The `args` of the plugin-facing vision requests the engine serves on
//! `vision.sock`, and the reply `vision.infer` returns.
//!
//! A plugin's vision request crosses three parties: the SDK builds it, the
//! plugin host forwards it to the engine unchanged, and the engine decodes it.
//! Each shape lives here once, so the Rust SDK and the engine build and read
//! the same bytes; the Python SDK mirrors them field for field.
//!
//! A contract payload rides as a msgpack binary field holding its
//! [`crate::framebus`] encoding — the form the engine's own pushes already use
//! (`vision.deliver {descriptor}`, `vision.deliver_detection {batch}`) — so it
//! crosses byte for byte and its version is checked where it is decoded:
//!
//! | method | args |
//! |---|---|
//! | `vision.publish_detection` | `{batch: bin(DetectionBatch)}` |
//! | `vision.register_model` | `{model: bin(ModelMetadata)}` |
//! | `vision.infer` | `{model_id: str, descriptor: bin(FrameDescriptor)}`; reply `{batch: bin(DetectionBatch)}` |
//! | `vision.designate_track` | `{camera_id: str, bbox: {x, y, width, height}, class_label?: str, confidence?: num}` |
//!
//! `vision.designate_track` is a flat map instead, because the control plane's
//! click-to-follow route sends the same request built from the operator's JSON
//! pick.

use rmpv::Value;
use thiserror::Error;

use crate::framebus::{BoundingBox, Detection, DetectionBatch, FrameDescriptor, ModelMetadata};

/// A vision request or reply that does not have its method's shape.
#[derive(Debug, Error)]
pub enum VisionArgsError {
    #[error("vision args are not a map")]
    NotAMap,
    #[error("missing `{0}`")]
    Missing(&'static str),
    #[error("`{field}` does not decode: {reason}")]
    Decode { field: &'static str, reason: String },
    #[error("`{field}` does not encode: {reason}")]
    Encode { field: &'static str, reason: String },
}

/// `vision.publish_detection` args: `{batch}`.
pub fn publish_detection_args(batch: &DetectionBatch) -> Result<Value, VisionArgsError> {
    Ok(map(vec![("batch", blob("batch", batch.to_msgpack())?)]))
}

/// Decode `vision.publish_detection` args, rejecting a batch whose version
/// this build does not speak.
pub fn decode_publish_detection(args: &Value) -> Result<DetectionBatch, VisionArgsError> {
    DetectionBatch::from_msgpack(binary(args, "batch")?).map_err(|e| decode_err("batch", e))
}

/// `vision.register_model` args: `{model}`.
pub fn register_model_args(model: &ModelMetadata) -> Result<Value, VisionArgsError> {
    Ok(map(vec![("model", blob("model", model.to_msgpack())?)]))
}

/// Decode `vision.register_model` args.
pub fn decode_register_model(args: &Value) -> Result<ModelMetadata, VisionArgsError> {
    ModelMetadata::from_msgpack(binary(args, "model")?).map_err(|e| decode_err("model", e))
}

/// A decoded `vision.infer` request: the model to run and the frame to run it
/// on, named by the descriptor the engine published for it.
#[derive(Debug, Clone, PartialEq)]
pub struct InferRequest {
    pub model_id: String,
    pub frame: FrameDescriptor,
}

/// `vision.infer` args: `{model_id, descriptor}`. The pixels stay in the ring
/// the descriptor names.
pub fn infer_args(model_id: &str, frame: &FrameDescriptor) -> Result<Value, VisionArgsError> {
    Ok(map(vec![
        ("model_id", Value::from(model_id)),
        ("descriptor", blob("descriptor", frame.to_msgpack())?),
    ]))
}

/// Decode `vision.infer` args, rejecting a descriptor whose version this build
/// does not speak.
pub fn decode_infer(args: &Value) -> Result<InferRequest, VisionArgsError> {
    let model_id = field(args, "model_id")?
        .as_str()
        .ok_or_else(|| decode_err("model_id", "not a string"))?
        .to_string();
    let frame = FrameDescriptor::from_msgpack(binary(args, "descriptor")?)
        .map_err(|e| decode_err("descriptor", e))?;
    Ok(InferRequest { model_id, frame })
}

/// The `vision.infer` reply: `{batch}`, the model's detections on that frame.
pub fn infer_reply(batch: &DetectionBatch) -> Result<Value, VisionArgsError> {
    publish_detection_args(batch)
}

/// Decode the `vision.infer` reply.
pub fn decode_infer_reply(args: &Value) -> Result<DetectionBatch, VisionArgsError> {
    decode_publish_detection(args)
}

/// A decoded `vision.designate_track` request: the camera whose tracker locks,
/// and the box it locks onto.
#[derive(Debug, Clone, PartialEq)]
pub struct DesignateTrack {
    pub camera_id: String,
    pub target: Detection,
}

/// `vision.designate_track` args from the detection to lock onto. Only its box,
/// label and confidence cross; the box is required.
pub fn designate_track_args(camera_id: &str, target: &Detection) -> Result<Value, VisionArgsError> {
    let bbox = target.bbox.ok_or(VisionArgsError::Missing("bbox"))?;
    Ok(map(vec![
        ("camera_id", Value::from(camera_id)),
        (
            "bbox",
            map(vec![
                ("x", Value::from(f64::from(bbox.x))),
                ("y", Value::from(f64::from(bbox.y))),
                ("width", Value::from(f64::from(bbox.width))),
                ("height", Value::from(f64::from(bbox.height))),
            ]),
        ),
        ("class_label", Value::from(target.class_label.as_str())),
        ("confidence", Value::from(f64::from(target.confidence))),
    ]))
}

/// Decode `vision.designate_track` args. Box fields read with numeric coercion,
/// so an int- or float-encoded value decodes the same; a missing box field is
/// 0. `class_label` and `confidence` default to an empty label and full
/// confidence: the operator's pick overrides the auto-lock regardless.
pub fn decode_designate_track(args: &Value) -> Result<DesignateTrack, VisionArgsError> {
    let camera_id = field(args, "camera_id")?
        .as_str()
        .ok_or_else(|| decode_err("camera_id", "not a string"))?
        .to_string();
    let bbox = field(args, "bbox")?;
    if !bbox.is_map() {
        return Err(decode_err("bbox", "not a map"));
    }
    let side = |k: &'static str| field(bbox, k).ok().and_then(number).unwrap_or(0.0);
    let target = Detection {
        bbox: Some(BoundingBox {
            x: side("x"),
            y: side("y"),
            width: side("width"),
            height: side("height"),
        }),
        class_label: field(args, "class_label")
            .ok()
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        confidence: field(args, "confidence")
            .ok()
            .and_then(number)
            .unwrap_or(1.0),
        track_id: None,
        assoc_confidence: None,
        lock_state: None,
        attributes: None,
        mask: None,
        keypoints: None,
        depth: None,
        world_pos: None,
    };
    Ok(DesignateTrack { camera_id, target })
}

fn map(pairs: Vec<(&str, Value)>) -> Value {
    Value::Map(
        pairs
            .into_iter()
            .map(|(k, v)| (Value::from(k), v))
            .collect(),
    )
}

fn blob(
    field: &'static str,
    encoded: Result<Vec<u8>, rmp_serde::encode::Error>,
) -> Result<Value, VisionArgsError> {
    encoded
        .map(Value::Binary)
        .map_err(|e| VisionArgsError::Encode {
            field,
            reason: e.to_string(),
        })
}

fn field<'a>(args: &'a Value, key: &'static str) -> Result<&'a Value, VisionArgsError> {
    let Value::Map(entries) = args else {
        return Err(VisionArgsError::NotAMap);
    };
    entries
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
        .ok_or(VisionArgsError::Missing(key))
}

fn binary<'a>(args: &'a Value, key: &'static str) -> Result<&'a [u8], VisionArgsError> {
    match field(args, key)? {
        Value::Binary(bytes) => Ok(bytes),
        _ => Err(decode_err(key, "not binary")),
    }
}

fn decode_err(field: &'static str, reason: impl ToString) -> VisionArgsError {
    VisionArgsError::Decode {
        field,
        reason: reason.to_string(),
    }
}

/// Any msgpack number as f32.
fn number(v: &Value) -> Option<f32> {
    v.as_f64()
        .or_else(|| v.as_i64().map(|i| i as f64))
        .or_else(|| v.as_u64().map(|u| u as f64))
        .map(|f| f as f32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framebus::VISION_DETECTION_VERSION;

    #[test]
    fn a_batch_of_an_unknown_version_is_refused_at_decode() {
        let batch = DetectionBatch {
            v: VISION_DETECTION_VERSION + 1,
            model_id: "m".into(),
            camera_id: "c".into(),
            frame_id: 1,
            ts_ms: 0,
            frame_width: 0,
            frame_height: 0,
            detections: vec![],
        };
        let args = publish_detection_args(&batch).unwrap();
        assert!(matches!(
            decode_publish_detection(&args),
            Err(VisionArgsError::Decode { field: "batch", .. })
        ));
    }

    #[test]
    fn designate_reads_integer_boxes_and_defaults_label_and_confidence() {
        // The control route builds this map from the operator's JSON pick,
        // where a whole-pixel box arrives as integers.
        let args = map(vec![
            ("camera_id", Value::from("cam-0")),
            (
                "bbox",
                map(vec![
                    ("x", Value::from(10)),
                    ("y", Value::from(20u64)),
                    ("width", Value::from(30.5)),
                    ("height", Value::from(40)),
                ]),
            ),
        ]);
        let d = decode_designate_track(&args).unwrap();
        assert_eq!(d.camera_id, "cam-0");
        assert_eq!(
            d.target.bbox,
            Some(BoundingBox {
                x: 10.0,
                y: 20.0,
                width: 30.5,
                height: 40.0
            })
        );
        assert_eq!(d.target.class_label, "");
        assert_eq!(d.target.confidence, 1.0);
    }

    #[test]
    fn designate_without_a_box_is_refused_on_both_ends() {
        let args = map(vec![("camera_id", Value::from("cam-0"))]);
        assert!(matches!(
            decode_designate_track(&args),
            Err(VisionArgsError::Missing("bbox"))
        ));
        let boxless = Detection {
            bbox: None,
            class_label: "person".into(),
            confidence: 0.9,
            track_id: None,
            assoc_confidence: None,
            lock_state: None,
            attributes: None,
            mask: None,
            keypoints: None,
            depth: None,
            world_pos: None,
        };
        assert!(matches!(
            designate_track_args("cam-0", &boxless),
            Err(VisionArgsError::Missing("bbox"))
        ));
    }
}
