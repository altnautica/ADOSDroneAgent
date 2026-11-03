//! Per-detection appearance embeddings on the detection wire.
//!
//! A plugin that subscribes with [`DetectionSubscription::embeddings`] receives,
//! on each box-bearing detection, the attributes [`EMBEDDING_ATTR`] (an
//! L2-normalized float list) and [`EMBEDDING_MODEL_ATTR`] (what produced it:
//! the engine's re-id model id, or [`HISTOGRAM_EMBEDDING`] for the model-free
//! colour descriptor). Embeddings from different sources differ in length and
//! meaning, so compare two only when their sources are equal. A detection
//! without them (box-less, or a frame the engine could not embed) is
//! motion-only.

use ados_protocol::framebus::Detection;

/// Detection attribute carrying the L2-normalized embedding.
pub const EMBEDDING_ATTR: &str = "embedding";
/// Detection attribute naming the embedding source.
pub const EMBEDDING_MODEL_ATTR: &str = "embedding_model";
/// The embedding source name of the model-free colour histogram.
pub const HISTOGRAM_EMBEDDING: &str = "histogram";

/// Options for [`super::VisionClient::subscribe_detections_with`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DetectionSubscription<'a> {
    /// Receive only this camera's batches; `None` receives every camera.
    pub camera_id: Option<&'a str>,
    /// Ask the engine for each detection's appearance embedding.
    pub embeddings: bool,
}

/// The appearance embedding a detection carries, with its source, or `None`
/// when the detection has none (or a malformed one).
pub fn detection_embedding(detection: &Detection) -> Option<(Vec<f32>, &str)> {
    let attrs = detection.attributes.as_ref()?;
    let values = attrs.get(EMBEDDING_ATTR)?.as_array()?;
    let embedding = values
        .iter()
        .map(|v| v.as_f64().map(|x| x as f32))
        .collect::<Option<Vec<f32>>>()?;
    let source = attrs.get(EMBEDDING_MODEL_ATTR)?.as_str()?;
    Some((embedding, source))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn with_attrs(attrs: Option<BTreeMap<String, rmpv::Value>>) -> Detection {
        Detection {
            bbox: None,
            class_label: "person".into(),
            confidence: 0.9,
            track_id: None,
            assoc_confidence: None,
            lock_state: None,
            attributes: attrs,
            mask: None,
            keypoints: None,
            depth: None,
            world_pos: None,
        }
    }

    #[test]
    fn reads_the_embedding_and_its_source() {
        let mut attrs = BTreeMap::new();
        attrs.insert(
            EMBEDDING_ATTR.to_string(),
            rmpv::Value::Array(vec![rmpv::Value::F32(0.6), rmpv::Value::F64(0.8)]),
        );
        attrs.insert(
            EMBEDDING_MODEL_ATTR.to_string(),
            rmpv::Value::from(HISTOGRAM_EMBEDDING),
        );
        let det = with_attrs(Some(attrs));
        let (emb, source) = detection_embedding(&det).expect("an embedding");
        assert_eq!(source, HISTOGRAM_EMBEDDING);
        assert!((emb[0] - 0.6).abs() < 1e-6 && (emb[1] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn a_detection_without_or_with_a_malformed_embedding_has_none() {
        assert!(detection_embedding(&with_attrs(None)).is_none());
        let mut no_source = BTreeMap::new();
        no_source.insert(
            EMBEDDING_ATTR.to_string(),
            rmpv::Value::Array(vec![rmpv::Value::F32(1.0)]),
        );
        assert!(detection_embedding(&with_attrs(Some(no_source))).is_none());
        let mut not_numbers = BTreeMap::new();
        not_numbers.insert(
            EMBEDDING_ATTR.to_string(),
            rmpv::Value::Array(vec![rmpv::Value::from("x")]),
        );
        not_numbers.insert(EMBEDDING_MODEL_ATTR.to_string(), rmpv::Value::from("m"));
        assert!(detection_embedding(&with_attrs(Some(not_numbers))).is_none());
    }
}
