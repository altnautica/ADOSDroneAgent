use super::*;

#[test]
fn model_metadata_round_trips() {
    let m = ModelMetadata {
        id: "com.example.weeds".into(),
        kind: ModelKind::Detection,
        execution: ModelExecution::EngineRun,
        input_width: 640,
        input_height: 480,
        input_format: FrameFormat::Rgb24,
        output_classes: vec!["weed".into(), "crop".into()],
        model_path: Some("/opt/ados/models/vision/weeds.onnx".into()),
        head: DetectionHead::Yolo8,
    };
    let bytes = m.to_msgpack().unwrap();
    assert_eq!(ModelMetadata::from_msgpack(&bytes).unwrap(), m);
}

#[test]
fn model_metadata_without_head_defaults_to_yolo8() {
    // A payload serialized before the `head` field existed (no `head` key)
    // must still decode, defaulting to the YOLOv8 head.
    let legacy = ModelMetadataLegacy {
        id: "com.example.old".into(),
        kind: ModelKind::Detection,
        execution: ModelExecution::EngineRun,
        input_width: 640,
        input_height: 640,
        input_format: FrameFormat::Rgb24,
        output_classes: vec!["uav".into()],
        model_path: None,
    };
    let bytes = rmp_serde::to_vec_named(&legacy).unwrap();
    let decoded = ModelMetadata::from_msgpack(&bytes).unwrap();
    assert_eq!(decoded.head, DetectionHead::Yolo8);
    assert_eq!(decoded.id, "com.example.old");
}

// The pre-`head` shape, to prove forward-compatibility of the new field.
#[derive(Serialize)]
struct ModelMetadataLegacy {
    id: String,
    kind: ModelKind,
    execution: ModelExecution,
    input_width: u32,
    input_height: u32,
    input_format: FrameFormat,
    output_classes: Vec<String>,
    model_path: Option<String>,
}

#[test]
fn detection_head_serializes_lowercase() {
    assert_eq!(
        serde_json::to_string(&DetectionHead::Yolo8).unwrap(),
        "\"yolo8\""
    );
    assert_eq!(
        serde_json::to_string(&DetectionHead::Yolo5).unwrap(),
        "\"yolo5\""
    );
}

#[test]
fn detection_batch_round_trips() {
    let b = DetectionBatch {
        v: VISION_DETECTION_VERSION,
        model_id: "com.example.weeds".into(),
        camera_id: "uvc-0".into(),
        frame_id: 7,
        ts_ms: 1_700_000_000_000,
        frame_width: 640,
        frame_height: 480,
        detections: vec![Detection {
            bbox: Some(BoundingBox {
                x: 12.0,
                y: 20.0,
                width: 64.0,
                height: 32.0,
            }),
            class_label: "weed".into(),
            confidence: 0.87,
            track_id: None,
            assoc_confidence: None,
            lock_state: None,
            attributes: None,
            mask: None,
            keypoints: None,
            depth: None,
            world_pos: None,
        }],
    };
    let bytes = b.to_msgpack().unwrap();
    assert_eq!(DetectionBatch::from_msgpack(&bytes).unwrap(), b);
}

#[test]
fn detection_rejects_a_future_version() {
    // A batch stamped with a version this build does not speak must fail the
    // decode loudly rather than silently mis-parse.
    let batch = DetectionBatch {
        v: VISION_DETECTION_VERSION + 1,
        model_id: "m".into(),
        camera_id: "c".into(),
        frame_id: 1,
        ts_ms: 0,
        frame_width: 640,
        frame_height: 480,
        detections: vec![],
    };
    let bytes = batch.to_msgpack().unwrap();
    assert!(matches!(
        DetectionBatch::from_msgpack(&bytes),
        Err(DetectionBatchError::Version { .. })
    ));
}

#[test]
fn detection_missing_version_fails_decode() {
    // A payload that predates the version field (no `v` key) must fail to
    // decode: the field is required, with no serde default.
    #[derive(Serialize)]
    struct NoVersionBatch {
        model_id: String,
        camera_id: String,
        frame_id: u64,
        ts_ms: i64,
        detections: Vec<Detection>,
    }
    let old = NoVersionBatch {
        model_id: "m".into(),
        camera_id: "c".into(),
        frame_id: 1,
        ts_ms: 0,
        detections: vec![],
    };
    let bytes = rmp_serde::to_vec_named(&old).unwrap();
    assert!(matches!(
        DetectionBatch::from_msgpack(&bytes),
        Err(DetectionBatchError::Decode(_))
    ));
}

#[test]
fn detection_lock_fields_round_trip_present() {
    let d = Detection {
        bbox: Some(BoundingBox {
            x: 1.0,
            y: 2.0,
            width: 10.0,
            height: 20.0,
        }),
        class_label: "target".into(),
        confidence: 0.91,
        track_id: Some(42),
        assoc_confidence: Some(0.73),
        lock_state: Some(LockState::Uncertain),
        attributes: None,
        mask: None,
        keypoints: None,
        depth: None,
        world_pos: None,
    };
    let bytes = rmp_serde::to_vec_named(&d).unwrap();
    let back: Detection = rmp_serde::from_slice(&bytes).unwrap();
    assert_eq!(back, d);
}

#[test]
fn detection_lock_state_serializes_lowercase() {
    let s = rmp_serde::to_vec_named(&LockState::Locked).unwrap();
    assert_eq!(rmp_serde::from_slice::<String>(&s).unwrap(), "locked");
    let s = rmp_serde::to_vec_named(&LockState::Lost).unwrap();
    assert_eq!(rmp_serde::from_slice::<String>(&s).unwrap(), "lost");
}

#[test]
fn detection_attributes_round_trip() {
    // The open self-describing attributes map survives the msgpack wire.
    let mut attrs = BTreeMap::new();
    attrs.insert("depth_m".to_string(), rmpv::Value::from(4.2_f64));
    attrs.insert("source".to_string(), rmpv::Value::from("plugin"));
    let d = Detection {
        bbox: Some(BoundingBox {
            x: 1.0,
            y: 2.0,
            width: 3.0,
            height: 4.0,
        }),
        class_label: "target".into(),
        confidence: 0.9,
        track_id: Some(7),
        assoc_confidence: None,
        lock_state: Some(LockState::Locked),
        attributes: Some(attrs.clone()),
        mask: None,
        keypoints: None,
        depth: None,
        world_pos: None,
    };
    let bytes = rmp_serde::to_vec_named(&d).unwrap();
    let back: Detection = rmp_serde::from_slice(&bytes).unwrap();
    assert_eq!(back, d);
    assert_eq!(back.attributes, Some(attrs));
}

#[test]
fn detection_without_attributes_defaults_none() {
    // A producer predating `attributes` (no such key) still decodes, with
    // attributes defaulting to None; and a detection with None skips the
    // key on the wire (no per-detection overhead in the common case).
    #[derive(Serialize)]
    struct NoAttrsDetection {
        bbox: BoundingBox,
        class_label: String,
        confidence: f32,
        track_id: Option<u64>,
        assoc_confidence: Option<f32>,
        lock_state: Option<LockState>,
    }
    let old = NoAttrsDetection {
        bbox: BoundingBox {
            x: 0.0,
            y: 0.0,
            width: 5.0,
            height: 5.0,
        },
        class_label: "target".into(),
        confidence: 0.5,
        track_id: None,
        assoc_confidence: None,
        lock_state: None,
    };
    let bytes = rmp_serde::to_vec_named(&old).unwrap();
    let back: Detection = rmp_serde::from_slice(&bytes).unwrap();
    assert_eq!(back.attributes, None);
}

#[test]
fn detection_new_fields_default_when_absent() {
    // A msgpack named-map written by an old producer that predates the
    // lock fields must still decode (the new fields default to None).
    #[derive(Serialize)]
    struct OldDetection {
        bbox: BoundingBox,
        class_label: String,
        confidence: f32,
        track_id: Option<u64>,
    }
    let old = OldDetection {
        bbox: BoundingBox {
            x: 0.0,
            y: 0.0,
            width: 5.0,
            height: 5.0,
        },
        class_label: "weed".into(),
        confidence: 0.5,
        track_id: Some(7),
    };
    let bytes = rmp_serde::to_vec_named(&old).unwrap();
    let back: Detection = rmp_serde::from_slice(&bytes).unwrap();
    assert_eq!(back.track_id, Some(7));
    assert_eq!(back.assoc_confidence, None);
    assert_eq!(back.lock_state, None);
}

#[test]
fn detection_new_fields_skipped_for_old_readers() {
    // A new producer's bytes must still decode for a reader that only
    // knows the old fields (forward compatibility).
    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct OldReader {
        bbox: BoundingBox,
        class_label: String,
        confidence: f32,
        #[serde(default)]
        track_id: Option<u64>,
    }
    let d = Detection {
        bbox: Some(BoundingBox {
            x: 1.0,
            y: 1.0,
            width: 2.0,
            height: 2.0,
        }),
        class_label: "target".into(),
        confidence: 0.8,
        track_id: Some(3),
        assoc_confidence: Some(0.4),
        lock_state: Some(LockState::Lost),
        attributes: None,
        mask: None,
        keypoints: None,
        depth: None,
        world_pos: None,
    };
    let bytes = rmp_serde::to_vec_named(&d).unwrap();
    let back: OldReader = rmp_serde::from_slice(&bytes).unwrap();
    assert_eq!(back.track_id, Some(3));
    assert_eq!(back.class_label, "target");
}

#[test]
fn enums_serialize_to_expected_strings() {
    let k = rmp_serde::to_vec_named(&ModelKind::Tracking).unwrap();
    assert_eq!(rmp_serde::from_slice::<String>(&k).unwrap(), "tracking");
    let e = rmp_serde::to_vec_named(&ModelExecution::PluginSide).unwrap();
    assert_eq!(rmp_serde::from_slice::<String>(&e).unwrap(), "plugin_side");
}

#[test]
fn model_kind_new_variants_serialize_lowercase() {
    for (kind, want) in [
        (ModelKind::Pose, "pose"),
        (ModelKind::Depth, "depth"),
        (ModelKind::Reid, "reid"),
    ] {
        let bytes = rmp_serde::to_vec_named(&kind).unwrap();
        assert_eq!(rmp_serde::from_slice::<String>(&bytes).unwrap(), want);
    }
}

#[test]
fn percept_typed_fields_round_trip() {
    // A rich percept carrying a mask, keypoints, a depth, and a world
    // position survives the msgpack wire byte-for-byte.
    let d = Detection {
        bbox: Some(BoundingBox {
            x: 10.0,
            y: 12.0,
            width: 30.0,
            height: 40.0,
        }),
        class_label: "person".into(),
        confidence: 0.88,
        track_id: Some(9),
        assoc_confidence: Some(0.7),
        lock_state: Some(LockState::Locked),
        attributes: None,
        mask: Some(vec![[10.0, 12.0], [40.0, 12.0], [40.0, 52.0], [10.0, 52.0]]),
        keypoints: Some(vec![
            Keypoint {
                x: 25.0,
                y: 18.0,
                confidence: 0.9,
            },
            Keypoint {
                x: 25.0,
                y: 40.0,
                confidence: 0.6,
            },
        ]),
        depth: Some(4.5),
        world_pos: Some([12.5, -3.0, 1.2]),
    };
    let bytes = rmp_serde::to_vec_named(&d).unwrap();
    let back: Detection = rmp_serde::from_slice(&bytes).unwrap();
    assert_eq!(back, d);
    assert_eq!(back.mask.as_ref().unwrap().len(), 4);
    assert_eq!(back.keypoints.as_ref().unwrap().len(), 2);
    assert_eq!(back.depth, Some(4.5));
    assert_eq!(back.world_pos, Some([12.5, -3.0, 1.2]));
}

#[test]
fn boxless_percept_round_trips() {
    // A percept with no 2D box (a mask/pose/depth-only reading) still
    // decodes: bbox is absent on the wire and reads back as None.
    let d = Detection {
        bbox: None,
        class_label: "region".into(),
        confidence: 0.5,
        track_id: None,
        assoc_confidence: None,
        lock_state: None,
        attributes: None,
        mask: Some(vec![[0.0, 0.0], [100.0, 0.0], [50.0, 100.0]]),
        keypoints: None,
        depth: Some(2.0),
        world_pos: None,
    };
    let bytes = rmp_serde::to_vec_named(&d).unwrap();
    let back: Detection = rmp_serde::from_slice(&bytes).unwrap();
    assert_eq!(back, d);
    assert_eq!(back.bbox, None);
}

#[test]
fn detection_predating_percept_fields_defaults_none() {
    // A producer that predates the mask/keypoints/depth/world_pos fields
    // (no such keys, and a required box) still decodes; the new typed
    // fields default to None.
    #[derive(Serialize)]
    struct PrePerceptDetection {
        bbox: BoundingBox,
        class_label: String,
        confidence: f32,
        track_id: Option<u64>,
        assoc_confidence: Option<f32>,
        lock_state: Option<LockState>,
    }
    let old = PrePerceptDetection {
        bbox: BoundingBox {
            x: 0.0,
            y: 0.0,
            width: 5.0,
            height: 5.0,
        },
        class_label: "target".into(),
        confidence: 0.6,
        track_id: Some(1),
        assoc_confidence: None,
        lock_state: Some(LockState::Locked),
    };
    let bytes = rmp_serde::to_vec_named(&old).unwrap();
    let back: Detection = rmp_serde::from_slice(&bytes).unwrap();
    assert_eq!(
        back.bbox,
        Some(BoundingBox {
            x: 0.0,
            y: 0.0,
            width: 5.0,
            height: 5.0,
        })
    );
    assert_eq!(back.mask, None);
    assert_eq!(back.keypoints, None);
    assert_eq!(back.depth, None);
    assert_eq!(back.world_pos, None);
}
