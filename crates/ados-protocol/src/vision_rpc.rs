//! The `args` of the plugin-facing vision requests the engine serves on
//! `vision.sock`, and the reply `vision.infer` returns.
//!
//! A plugin's vision request crosses three parties: the SDK builds it, the
//! plugin host forwards it to the engine, and the engine decodes it. Each shape
//! lives here once, so the Rust SDK and the engine build and read the same
//! bytes; the Python SDK mirrors them field for field.
//!
//! The host stamps the calling plugin's id into `vision.register_model` and
//! `vision.infer` as a string [`OWNER_FIELD`] before forwarding, so the engine
//! keeps each plugin's models apart. A plugin never sets it: the host replaces
//! whatever the request carried. When the host stops serving a plugin it sends
//! [`UNREGISTER_OWNER`] `{owner}` so the engine drops that plugin's models.
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

/// The group that may read the camera frame rings in `/dev/shm`. The engine
/// creates each ring `0640` in this group; a plugin unit joins it only while
/// its `vision.frame.read` grant is held.
pub const VISION_READERS_GROUP: &str = "ados-vision-readers";

/// The args field carrying the plugin id the host stamped on a request.
pub const OWNER_FIELD: &str = "owner";

/// Host-to-engine request dropping every model one plugin registered. Not a
/// plugin-facing method: no capability maps to it and the host never routes a
/// plugin's request to it.
pub const UNREGISTER_OWNER: &str = "vision.unregister_owner";

/// `args` with [`OWNER_FIELD`] set to `owner`, replacing any value the caller
/// put there. Non-map args come back as `{owner}` alone, which the engine then
/// refuses for the missing payload.
pub fn with_owner(args: &Value, owner: &str) -> Value {
    let mut entries: Vec<(Value, Value)> = match args {
        Value::Map(entries) => entries
            .iter()
            .filter(|(k, _)| k.as_str() != Some(OWNER_FIELD))
            .cloned()
            .collect(),
        _ => Vec::new(),
    };
    entries.push((Value::from(OWNER_FIELD), Value::from(owner)));
    Value::Map(entries)
}

/// The non-empty [`OWNER_FIELD`] a request carries.
pub fn owner_of(args: &Value) -> Result<&str, VisionArgsError> {
    field(args, OWNER_FIELD)?
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| decode_err(OWNER_FIELD, "not a non-empty string"))
}

/// [`UNREGISTER_OWNER`] args: `{owner}`.
pub fn unregister_owner_args(owner: &str) -> Value {
    map(vec![(OWNER_FIELD, Value::from(owner))])
}

/// The id a plugin's model or published batch carries on the engine: the
/// plugin's own id under its namespace (`<plugin_id>/<id>`), so a plugin can
/// neither replace nor pose as the engine's configured models or another
/// plugin's. An id the plugin already namespaced is kept as is.
pub fn owned_model_id(owner: &str, id: &str) -> String {
    match id
        .strip_prefix(owner)
        .and_then(|rest| rest.strip_prefix('/'))
    {
        Some(_) => id.to_string(),
        None => format!("{owner}/{id}"),
    }
}

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
/// so an int- or float-encoded value decodes the same. Every box field is
/// required and must be finite, `x`/`y` non-negative and `width`/`height`
/// positive: a NaN or zero-size box would seed a tracker that never associates
/// or predicts NaN. `class_label` defaults to empty and `confidence` to full,
/// clamped to `0..=1`: the operator's pick overrides the auto-lock regardless.
pub fn decode_designate_track(args: &Value) -> Result<DesignateTrack, VisionArgsError> {
    let camera_id = field(args, "camera_id")?
        .as_str()
        .ok_or_else(|| decode_err("camera_id", "not a string"))?
        .to_string();
    let bbox = field(args, "bbox")?;
    if !bbox.is_map() {
        return Err(decode_err("bbox", "not a map"));
    }
    let side = |k: &'static str| -> Result<f32, VisionArgsError> {
        let v = field(bbox, k)
            .ok()
            .and_then(number)
            .ok_or_else(|| decode_err("bbox", format!("`{k}` is missing or not a number")))?;
        if v.is_finite() {
            Ok(v)
        } else {
            Err(decode_err("bbox", format!("`{k}` is not finite")))
        }
    };
    let (x, y, width, height) = (side("x")?, side("y")?, side("width")?, side("height")?);
    if x < 0.0 || y < 0.0 {
        return Err(decode_err("bbox", "x and y must be non-negative"));
    }
    if width <= 0.0 || height <= 0.0 {
        return Err(decode_err("bbox", "width and height must be positive"));
    }
    let confidence = field(args, "confidence")
        .ok()
        .and_then(number)
        .filter(|c| c.is_finite())
        .unwrap_or(1.0)
        .clamp(0.0, 1.0);
    let target = Detection {
        bbox: Some(BoundingBox {
            x,
            y,
            width,
            height,
        }),
        class_label: field(args, "class_label")
            .ok()
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        confidence,
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

    #[test]
    fn designate_refuses_non_finite_and_empty_boxes_and_clamps_confidence() {
        let with = |x: f64, w: f64, conf: f64| {
            map(vec![
                ("camera_id", Value::from("cam-0")),
                (
                    "bbox",
                    map(vec![
                        ("x", Value::from(x)),
                        ("y", Value::from(1.0)),
                        ("width", Value::from(w)),
                        ("height", Value::from(5.0)),
                    ]),
                ),
                ("confidence", Value::from(conf)),
            ])
        };
        for bad in [
            with(f64::NAN, 5.0, 0.5),
            with(f64::INFINITY, 5.0, 0.5),
            with(-1.0, 5.0, 0.5),
            with(1.0, 0.0, 0.5),
            with(1.0, -3.0, 0.5),
        ] {
            assert!(matches!(
                decode_designate_track(&bad),
                Err(VisionArgsError::Decode { field: "bbox", .. })
            ));
        }
        let d = decode_designate_track(&with(1.0, 5.0, 7.0)).unwrap();
        assert_eq!(d.target.confidence, 1.0);
        let d = decode_designate_track(&with(1.0, 5.0, -2.0)).unwrap();
        assert_eq!(d.target.confidence, 0.0);
    }

    #[test]
    fn the_stamped_owner_replaces_whatever_the_plugin_sent() {
        let spoofed = map(vec![
            ("model_id", Value::from("det")),
            (OWNER_FIELD, Value::from("com.example.victim")),
        ]);
        let stamped = with_owner(&spoofed, "com.example.caller");
        assert_eq!(owner_of(&stamped).unwrap(), "com.example.caller");
        let Value::Map(entries) = &stamped else {
            panic!("not a map");
        };
        assert_eq!(
            entries
                .iter()
                .filter(|(k, _)| k.as_str() == Some(OWNER_FIELD))
                .count(),
            1
        );
        assert_eq!(field(&stamped, "model_id").unwrap().as_str(), Some("det"));
        assert!(owner_of(&map(vec![(OWNER_FIELD, Value::from(""))])).is_err());
    }
}
