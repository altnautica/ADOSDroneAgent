use super::*;
use ados_protocol::framebus::{BoundingBox, DetectionHead, ModelKind};

const W: u32 = 32;
const H: u32 = 32;

/// A detector that always returns one box, and an embedder that returns a
/// fixed raw vector (so its L2 normalization is checkable).
struct FixedBackend;
struct FixedDetector;
struct FixedEmbedder;

fn the_box() -> Detection {
    Detection {
        bbox: Some(BoundingBox {
            x: 4.0,
            y: 4.0,
            width: 12.0,
            height: 20.0,
        }),
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
    }
}

impl LoadedModel for FixedDetector {
    fn infer(&self, _f: &[u8], _w: u32, _h: u32, _fmt: FrameFormat) -> Result<Vec<Detection>> {
        Ok(vec![the_box()])
    }
}

impl LoadedModel for FixedEmbedder {
    fn infer(&self, _f: &[u8], _w: u32, _h: u32, _fmt: FrameFormat) -> Result<Vec<Detection>> {
        Ok(Vec::new())
    }
    fn embed(&self, _c: &[u8], _w: u32, _h: u32, _f: FrameFormat) -> Result<Option<Vec<f32>>> {
        Ok(Some(vec![3.0, 4.0]))
    }
}

impl VisionBackend for FixedBackend {
    fn load(&self, meta: &ModelMetadata) -> Result<Box<dyn LoadedModel>> {
        Ok(match meta.kind {
            ModelKind::Reid => Box::new(FixedEmbedder),
            _ => Box::new(FixedDetector),
        })
    }
    fn name(&self) -> &str {
        "fixed"
    }
}

fn model(id: &str, kind: ModelKind, w: u32, h: u32) -> ModelMetadata {
    ModelMetadata {
        id: id.into(),
        kind,
        execution: ModelExecution::EngineRun,
        input_width: w,
        input_height: h,
        input_format: FrameFormat::Rgb24,
        output_classes: vec!["person".into()],
        model_path: Some("/tmp/fixed".into()),
        head: DetectionHead::Yolo8,
    }
}

fn desc() -> FrameDescriptor {
    FrameDescriptor {
        v: ados_protocol::framebus::FRAMEBUS_DESCRIPTOR_VERSION,
        camera_id: "cam".into(),
        frame_id: 1,
        ts_ns: 1,
        ts_sensor_ns: None,
        exposure_ns: None,
        trigger_seq: None,
        timebase: ados_protocol::framebus::TIMEBASE_HOST,
        width: W,
        height: H,
        format: FrameFormat::Rgb24,
        seq: 1,
        slot: 0,
        shm_name: String::new(),
        byte_len: W * H * 3,
    }
}

fn frame() -> Vec<u8> {
    // A red upper half over a blue lower half.
    let mut f = Vec::with_capacity((W * H * 3) as usize);
    for y in 0..H {
        let px = if y < H / 2 {
            [200, 20, 20]
        } else {
            [20, 20, 200]
        };
        for _ in 0..W {
            f.extend_from_slice(&px);
        }
    }
    f
}

async fn engine_with(reid: bool) -> Arc<VisionEngine> {
    let e = VisionEngine::with_tracker_reid(
        Box::new(FixedBackend),
        4,
        false,
        TrackerConfig::default(),
        false,
        reid.then(|| "reid".to_string()),
    );
    e.register_model(model("det", ModelKind::Detection, 32, 32))
        .await
        .unwrap();
    if reid {
        e.register_model(model("reid", ModelKind::Reid, 8, 16))
            .await
            .unwrap();
    }
    e
}

fn embedding_of(d: &Detection) -> Option<(Vec<f32>, String)> {
    let attrs = d.attributes.as_ref()?;
    let emb = attrs
        .get(EMBEDDING_ATTR)?
        .as_array()?
        .iter()
        .map(|v| v.as_f64().map(|x| x as f32))
        .collect::<Option<Vec<f32>>>()?;
    let src = attrs.get(EMBEDDING_MODEL_ATTR)?.as_str()?.to_string();
    Some((emb, src))
}

#[tokio::test]
async fn no_embeddings_are_extracted_or_sent_until_a_subscriber_asks() {
    let e = engine_with(false).await;
    assert!(!e.embeddings_requested());
    let batch = e.infer_and_publish("det", &desc(), &frame()).await.unwrap();
    assert!(batch.detections[0].attributes.is_none());
}

#[tokio::test]
async fn without_a_reid_model_the_histogram_embedding_rides_the_wire() {
    let e = engine_with(false).await;
    let interest = e.request_embeddings();
    assert!(e.embeddings_requested());
    let batch = e.infer_and_publish("det", &desc(), &frame()).await.unwrap();
    let (emb, src) = embedding_of(&batch.detections[0]).expect("an embedding");
    assert_eq!(src, HISTOGRAM_EMBEDDING);
    assert_eq!(emb.len(), ados_vision::tracker::APPEARANCE_DIM);
    let norm: f32 = emb.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!((norm - 1.0).abs() < 1e-3, "unit length, got {norm}");

    // Withdrawing the request stops the extraction.
    drop(interest);
    assert!(!e.embeddings_requested());
    let batch = e.infer_and_publish("det", &desc(), &frame()).await.unwrap();
    assert!(batch.detections[0].attributes.is_none());
}

#[tokio::test]
async fn a_loaded_reid_model_supplies_the_embedding_normalized() {
    let e = engine_with(true).await;
    let _interest = e.request_embeddings();
    let batch = e.infer_and_publish("det", &desc(), &frame()).await.unwrap();
    let (emb, src) = embedding_of(&batch.detections[0]).expect("an embedding");
    assert_eq!(src, "reid");
    assert!((emb[0] - 0.6).abs() < 1e-5 && (emb[1] - 0.8).abs() < 1e-5);
}

#[test]
fn strip_removes_only_the_embedding_attributes() {
    let mut only = the_box();
    let mut both = the_box();
    let apps = embeddings::FrameAppearances {
        per_detection: vec![Some(Appearance::from_features(vec![1.0, 0.0]))],
        source: HISTOGRAM_EMBEDDING.into(),
    };
    embeddings::stamp_embeddings(std::slice::from_mut(&mut only), &apps);
    embeddings::stamp_embeddings(std::slice::from_mut(&mut both), &apps);
    both.attributes
        .as_mut()
        .unwrap()
        .insert("plate".into(), rmpv::Value::from("AB12"));

    let mut batch = DetectionBatch {
        v: VISION_DETECTION_VERSION,
        model_id: "det".into(),
        camera_id: "cam".into(),
        frame_id: 1,
        ts_ms: 0,
        frame_width: W,
        frame_height: H,
        detections: vec![only, both],
    };
    assert!(strip_embeddings(&mut batch));
    // A detection whose only attributes were the embedding reads as before.
    assert!(batch.detections[0].attributes.is_none());
    let rest = batch.detections[1].attributes.as_ref().unwrap();
    assert_eq!(rest.len(), 1);
    assert!(rest.contains_key("plate"));
    // Nothing left to strip.
    assert!(!strip_embeddings(&mut batch));
}
