//! Self-describing model files.
//!
//! An exported graph may carry `ados.*` key/value metadata that states its own
//! head layout, class list and input size (the ONNX `metadata_props`). The
//! backend applies them on load so a model file decodes correctly no matter
//! which writer produced the `vision.detector` block: the file is the authority
//! on its own graph. Keys:
//!
//! - `ados.head`: `yolo8` | `yolo5` (the output-head layout).
//! - `ados.classes`: comma-separated class labels in output-index order. Used
//!   only when the configuration supplied no labels.
//! - `ados.input`: `<width>x<height>` of the model input.
//!
//! An unparseable value is ignored with a warning; the configured value stays.

use ados_protocol::framebus::{DetectionHead, ModelMetadata};

/// Metadata key for the output-head layout.
pub const HEAD_KEY: &str = "ados.head";
/// Metadata key for the comma-separated class labels.
pub const CLASSES_KEY: &str = "ados.classes";
/// Metadata key for the `<width>x<height>` input size.
pub const INPUT_KEY: &str = "ados.input";

/// Apply the `ados.*` properties `get` returns onto `meta`.
pub fn apply_model_props(meta: &mut ModelMetadata, get: impl Fn(&str) -> Option<String>) {
    if let Some(raw) = get(HEAD_KEY) {
        match parse_head(&raw) {
            Some(head) => meta.head = head,
            None => {
                tracing::warn!(model = %meta.id, value = %raw, "unknown ados.head in model file")
            }
        }
    }
    if meta.output_classes.is_empty() {
        if let Some(raw) = get(CLASSES_KEY) {
            meta.output_classes = raw
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
        }
    }
    if let Some(raw) = get(INPUT_KEY) {
        match parse_wxh(&raw) {
            Some((w, h)) => {
                meta.input_width = w;
                meta.input_height = h;
            }
            None => {
                tracing::warn!(model = %meta.id, value = %raw, "unparseable ados.input in model file")
            }
        }
    }
}

fn parse_head(raw: &str) -> Option<DetectionHead> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "yolo8" | "yolov8" => Some(DetectionHead::Yolo8),
        "yolo5" | "yolov5" => Some(DetectionHead::Yolo5),
        _ => None,
    }
}

fn parse_wxh(raw: &str) -> Option<(u32, u32)> {
    let (w, h) = raw.trim().split_once(['x', 'X'])?;
    let w: u32 = w.trim().parse().ok()?;
    let h: u32 = h.trim().parse().ok()?;
    (w > 0 && h > 0).then_some((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::framebus::{FrameFormat, ModelExecution, ModelKind};
    use std::collections::HashMap;

    fn meta(classes: &[&str]) -> ModelMetadata {
        ModelMetadata {
            id: "det".into(),
            kind: ModelKind::Detection,
            execution: ModelExecution::EngineRun,
            input_width: 640,
            input_height: 640,
            input_format: FrameFormat::Rgb24,
            output_classes: classes.iter().map(|s| s.to_string()).collect(),
            model_path: Some("/tmp/det.onnx".into()),
            head: DetectionHead::Yolo8,
        }
    }

    fn props(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn a_self_describing_file_sets_head_classes_and_input() {
        let p = props(&[
            (HEAD_KEY, "yolo5"),
            (CLASSES_KEY, "person"),
            (INPUT_KEY, "416x320"),
        ]);
        let mut m = meta(&[]);
        apply_model_props(&mut m, |k| p.get(k).cloned());
        assert_eq!(m.head, DetectionHead::Yolo5);
        assert_eq!(m.output_classes, vec!["person".to_string()]);
        assert_eq!((m.input_width, m.input_height), (416, 320));
    }

    #[test]
    fn configured_classes_win_over_the_file() {
        let p = props(&[(CLASSES_KEY, "person")]);
        let mut m = meta(&["person", "bicycle"]);
        apply_model_props(&mut m, |k| p.get(k).cloned());
        assert_eq!(m.output_classes.len(), 2);
    }

    #[test]
    fn a_file_without_props_or_with_bad_values_changes_nothing() {
        let mut m = meta(&["person"]);
        apply_model_props(&mut m, |_| None);
        assert_eq!(m, meta(&["person"]));

        let p = props(&[(HEAD_KEY, "detr"), (INPUT_KEY, "0x640")]);
        apply_model_props(&mut m, |k| p.get(k).cloned());
        assert_eq!(m.head, DetectionHead::Yolo8);
        assert_eq!((m.input_width, m.input_height), (640, 640));
    }
}
