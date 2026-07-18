//! YOLO detection-head decoding for the Rust (ONNX) backend.
//!
//! This is the Rust twin of the Python sidecar's `decode_yolo_detections`: it
//! turns a model's raw output tensor into [`Detection`]s in source-frame pixels,
//! selecting the layout by [`DetectionHead`].
//!
//! * [`DetectionHead::Yolo8`] — the transposed `[1, 4+nc, anchors]` head: four
//!   box rows (cx, cy, w, h) then one score row per class, no objectness. This
//!   is the ultralytics YOLOv8/v11 export.
//! * [`DetectionHead::Yolo5`] — the legacy `[1, anchors, 5+nc]` head: per anchor
//!   `[cx, cy, w, h, objectness, class_scores...]` (YOLOv5/v7).
//!
//! The decoder is orientation robust: the raw output is a flat row-major buffer
//! with a logical 2D shape `(rows, cols)` (after the batch dim is dropped), and
//! the decoder transposes a head delivered as `(anchors, features)`
//! automatically. Boxes are in the model's input resolution and scale to the
//! source frame by `frame / input`; survivors of the confidence gate pass
//! class-agnostic non-maximum suppression.

use ados_protocol::framebus::{BoundingBox, Detection, DetectionHead};

/// Parameters for a single decode pass.
pub struct DecodeParams<'a> {
    pub head: DetectionHead,
    /// Class labels in output-index order. Empty ⇒ one unnamed class.
    pub labels: &'a [String],
    /// The model's input resolution (boxes are in these pixels).
    pub input_w: u32,
    pub input_h: u32,
    /// The source frame resolution (boxes scale up/down to these pixels).
    pub frame_w: u32,
    pub frame_h: u32,
    pub conf_threshold: f32,
    pub nms_iou: f32,
}

/// A center-form box in model-input pixels with its score and class index.
struct Cand {
    cx: f32,
    cy: f32,
    w: f32,
    h: f32,
    score: f32,
    class: usize,
}

/// Decode a raw model output (`data`, row-major, logical shape `(rows, cols)`
/// after the batch dim is dropped) into detections in source-frame pixels.
pub fn decode(data: &[f32], rows: usize, cols: usize, p: &DecodeParams) -> Vec<Detection> {
    if rows == 0 || cols == 0 || data.len() < rows * cols {
        return Vec::new();
    }
    let nc = p.labels.len().max(1);
    let cands = match p.head {
        DetectionHead::Yolo8 => decode_v8(data, rows, cols, nc, p.conf_threshold),
        DetectionHead::Yolo5 => decode_v5(data, rows, cols, nc, p.conf_threshold),
    };
    if cands.is_empty() {
        return Vec::new();
    }

    let scale_x = if p.input_w != 0 {
        p.frame_w as f32 / p.input_w as f32
    } else {
        1.0
    };
    let scale_y = if p.input_h != 0 {
        p.frame_h as f32 / p.input_h as f32
    } else {
        1.0
    };

    // Corner-form boxes in source-frame pixels, paired with score/class.
    let boxes: Vec<(BoundingBox, f32, usize)> = cands
        .into_iter()
        .map(|c| {
            let x = (c.cx - c.w / 2.0) * scale_x;
            let y = (c.cy - c.h / 2.0) * scale_y;
            (
                BoundingBox {
                    x,
                    y,
                    width: c.w * scale_x,
                    height: c.h * scale_y,
                },
                c.score,
                c.class,
            )
        })
        .collect();

    let keep = nms(&boxes, p.nms_iou);
    // `nms` returns indices in descending score order; map them to detections.
    let mut out = Vec::with_capacity(keep.len());
    for i in keep {
        let (bbox, score, class) = boxes[i];
        let label = p
            .labels
            .get(class)
            .cloned()
            .unwrap_or_else(|| class.to_string());
        out.push(Detection {
            bbox: Some(bbox),
            class_label: label,
            confidence: score,
            track_id: None,
            assoc_confidence: None,
            lock_state: None,
            attributes: None,
            mask: None,
            keypoints: None,
            depth: None,
            world_pos: None,
        });
    }
    out
}

/// Preprocess a raw RGB24 frame into a model input tensor: bilinear-resize to
/// `(input_w, input_h)`, scale to `[0, 1]`, and lay it out channel-planar NCHW
/// (`[3, input_h, input_w]` flattened, R plane then G then B). Returns `None`
/// when a dimension is zero or the frame buffer is too small.
///
/// The simple stretch resize (not a letterbox) is deliberate: it matches the
/// `frame / input` box scale-back in [`decode`] and the Python sidecar's own
/// scaling, so the Rust ONNX path and the sidecar path stay box-consistent. A
/// letterbox would have to land in both decoders at once.
pub fn preprocess_rgb24_nchw(
    frame: &[u8],
    frame_w: u32,
    frame_h: u32,
    input_w: u32,
    input_h: u32,
) -> Option<Vec<f32>> {
    let (fw, fh, iw, ih) = (
        frame_w as usize,
        frame_h as usize,
        input_w as usize,
        input_h as usize,
    );
    if fw == 0 || fh == 0 || iw == 0 || ih == 0 || frame.len() < fw * fh * 3 {
        return None;
    }
    let mut out = vec![0f32; 3 * ih * iw];
    let plane = ih * iw;
    let sx = fw as f32 / iw as f32;
    let sy = fh as f32 / ih as f32;
    let sample = |y: usize, x: usize, c: usize| frame[(y * fw + x) * 3 + c] as f32;
    for oy in 0..ih {
        let fy = ((oy as f32 + 0.5) * sy - 0.5).clamp(0.0, (fh - 1) as f32);
        let y0 = fy.floor() as usize;
        let y1 = (y0 + 1).min(fh - 1);
        let wy = fy - y0 as f32;
        for ox in 0..iw {
            let fx = ((ox as f32 + 0.5) * sx - 0.5).clamp(0.0, (fw - 1) as f32);
            let x0 = fx.floor() as usize;
            let x1 = (x0 + 1).min(fw - 1);
            let wx = fx - x0 as f32;
            for c in 0..3 {
                let top = sample(y0, x0, c) * (1.0 - wx) + sample(y0, x1, c) * wx;
                let bot = sample(y1, x0, c) * (1.0 - wx) + sample(y1, x1, c) * wx;
                out[c * plane + oy * iw + ox] = (top * (1.0 - wy) + bot * wy) / 255.0;
            }
        }
    }
    Some(out)
}

/// Read element `(r, c)` of the row-major buffer.
#[inline]
fn at(data: &[f32], cols: usize, r: usize, c: usize) -> f32 {
    data[r * cols + c]
}

/// Decode the transposed YOLOv8 head: features along one axis (`4+nc`), anchors
/// along the other, no objectness.
fn decode_v8(data: &[f32], rows: usize, cols: usize, nc: usize, conf: f32) -> Vec<Cand> {
    let feat_len = 4 + nc;
    // Orientation: which axis is the feature axis? Prefer an exact match, else
    // take the shorter axis as features (features ≪ anchors).
    let feat_major = if rows == feat_len {
        true
    } else if cols == feat_len {
        false
    } else {
        rows <= cols
    };
    let (n_feat, n_anchor) = if feat_major {
        (rows, cols)
    } else {
        (cols, rows)
    };
    if n_feat < 5 {
        return Vec::new();
    }
    let classes = (n_feat - 4).min(nc);
    // value(feature f, anchor a)
    let val = |f: usize, a: usize| -> f32 {
        if feat_major {
            at(data, cols, f, a)
        } else {
            at(data, cols, a, f)
        }
    };
    let mut out = Vec::new();
    for a in 0..n_anchor {
        let mut best = 0usize;
        let mut best_score = f32::MIN;
        for k in 0..classes {
            let s = val(4 + k, a);
            if s > best_score {
                best_score = s;
                best = k;
            }
        }
        if best_score < conf {
            continue;
        }
        out.push(Cand {
            cx: val(0, a),
            cy: val(1, a),
            w: val(2, a),
            h: val(3, a),
            score: best_score,
            class: best,
        });
    }
    out
}

/// Decode the legacy YOLOv5 head: per anchor `[cx,cy,w,h,obj,class_scores...]`.
fn decode_v5(data: &[f32], rows: usize, cols: usize, nc: usize, conf: f32) -> Vec<Cand> {
    let row_len = 5 + nc;
    // Orientation: which axis is the per-anchor feature row?
    let anchor_major = if cols == row_len {
        true
    } else if rows == row_len {
        false
    } else {
        rows >= cols
    };
    let (n_anchor, n_feat) = if anchor_major {
        (rows, cols)
    } else {
        (cols, rows)
    };
    if n_feat < 6 {
        return Vec::new();
    }
    let classes = (n_feat - 5).min(nc);
    let val = |a: usize, f: usize| -> f32 {
        if anchor_major {
            at(data, cols, a, f)
        } else {
            at(data, cols, f, a)
        }
    };
    let mut out = Vec::new();
    for a in 0..n_anchor {
        let obj = val(a, 4);
        let mut best = 0usize;
        let mut best_cls = f32::MIN;
        for k in 0..classes {
            let s = val(a, 5 + k);
            if s > best_cls {
                best_cls = s;
                best = k;
            }
        }
        let score = obj * best_cls;
        if score < conf {
            continue;
        }
        out.push(Cand {
            cx: val(a, 0),
            cy: val(a, 1),
            w: val(a, 2),
            h: val(a, 3),
            score,
            class: best,
        });
    }
    out
}

/// Class-agnostic greedy non-maximum suppression over `(bbox, score, class)`.
/// Returns kept indices into `boxes`, highest score first.
fn nms(boxes: &[(BoundingBox, f32, usize)], iou_threshold: f32) -> Vec<usize> {
    let mut order: Vec<usize> = (0..boxes.len()).collect();
    order.sort_by(|&a, &b| boxes[b].1.total_cmp(&boxes[a].1));
    let mut kept: Vec<usize> = Vec::new();
    'outer: for &i in &order {
        for &k in &kept {
            if iou(&boxes[i].0, &boxes[k].0) >= iou_threshold {
                continue 'outer;
            }
        }
        kept.push(i);
    }
    kept
}

/// Intersection-over-union of two corner-form boxes.
fn iou(a: &BoundingBox, b: &BoundingBox) -> f32 {
    let (ax2, ay2) = (a.x + a.width, a.y + a.height);
    let (bx2, by2) = (b.x + b.width, b.y + b.height);
    let ix1 = a.x.max(b.x);
    let iy1 = a.y.max(b.y);
    let ix2 = ax2.min(bx2);
    let iy2 = ay2.min(by2);
    let iw = (ix2 - ix1).max(0.0);
    let ih = (iy2 - iy1).max(0.0);
    let inter = iw * ih;
    if inter <= 0.0 {
        return 0.0;
    }
    let union = a.width * a.height + b.width * b.height - inter;
    if union > 0.0 {
        inter / union
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels() -> Vec<String> {
        vec!["drone".into(), "person".into()]
    }

    fn params(head: DetectionHead, labels: &[String], conf: f32) -> DecodeParams<'_> {
        DecodeParams {
            head,
            labels,
            input_w: 640,
            input_h: 640,
            frame_w: 640,
            frame_h: 640,
            conf_threshold: conf,
            nms_iou: 0.45,
        }
    }

    // (4+nc, anchors) = (6, 3) flattened row-major: cx,cy,w,h,score0,score1.
    fn v8_buffer() -> (Vec<f32>, usize, usize) {
        let rows: [[f32; 3]; 6] = [
            [100.0, 300.0, 10.0], // cx
            [100.0, 300.0, 10.0], // cy
            [40.0, 20.0, 5.0],    // w
            [40.0, 20.0, 5.0],    // h
            [0.90, 0.10, 0.05],   // drone
            [0.10, 0.80, 0.02],   // person
        ];
        let mut data = Vec::new();
        for r in rows {
            data.extend_from_slice(&r);
        }
        (data, 6, 3)
    }

    #[test]
    fn yolov8_decodes_two_boxes_no_objectness() {
        let l = labels();
        let (data, r, c) = v8_buffer();
        let dets = decode(&data, r, c, &params(DetectionHead::Yolo8, &l, 0.25));
        assert_eq!(dets.len(), 2);
        let drone = dets.iter().find(|d| d.class_label == "drone").unwrap();
        let drone_b = drone.bbox.as_ref().unwrap();
        assert!((drone_b.x - 80.0).abs() < 1e-3);
        assert!((drone_b.y - 80.0).abs() < 1e-3);
        assert!((drone_b.width - 40.0).abs() < 1e-3);
        assert!((drone.confidence - 0.90).abs() < 1e-4);
        let person = dets.iter().find(|d| d.class_label == "person").unwrap();
        assert!((person.bbox.as_ref().unwrap().x - 290.0).abs() < 1e-3);
        assert!((person.confidence - 0.80).abs() < 1e-4);
    }

    #[test]
    fn yolov8_is_orientation_robust() {
        // Transpose (6,3) -> (3,6) row-major and decode the same boxes.
        let l = labels();
        let (data, r, c) = v8_buffer();
        let mut t = vec![0f32; data.len()];
        for i in 0..r {
            for j in 0..c {
                t[j * r + i] = data[i * c + j];
            }
        }
        let dets = decode(&t, c, r, &params(DetectionHead::Yolo8, &l, 0.25));
        assert_eq!(dets.len(), 2);
        let got: std::collections::BTreeSet<_> =
            dets.iter().map(|d| d.class_label.clone()).collect();
        assert!(got.contains("drone") && got.contains("person"));
    }

    #[test]
    fn yolov8_misread_as_yolov5_differs() {
        let l = labels();
        let (data, r, c) = v8_buffer();
        let v8 = decode(&data, r, c, &params(DetectionHead::Yolo8, &l, 0.25));
        let v5 = decode(&data, r, c, &params(DetectionHead::Yolo5, &l, 0.25));
        assert_ne!(v8, v5);
    }

    #[test]
    fn yolov8_threshold_gate_drops_low_confidence() {
        let l = labels();
        let (data, r, c) = v8_buffer();
        let dets = decode(&data, r, c, &params(DetectionHead::Yolo8, &l, 0.95));
        assert!(dets.is_empty());
    }

    #[test]
    fn yolov5_decodes_with_objectness() {
        // (anchors, 5+nc) = (3, 7): cx,cy,w,h,obj,score0,score1.
        let l = labels();
        let rows: [[f32; 7]; 3] = [
            [100.0, 100.0, 40.0, 40.0, 0.90, 0.95, 0.10], // 0.855 drone
            [300.0, 300.0, 20.0, 20.0, 0.80, 0.10, 0.90], // 0.720 person
            [10.0, 10.0, 5.0, 5.0, 0.10, 0.50, 0.50],     // 0.05 dropped
        ];
        let mut data = Vec::new();
        for r in rows {
            data.extend_from_slice(&r);
        }
        let dets = decode(&data, 3, 7, &params(DetectionHead::Yolo5, &l, 0.25));
        assert_eq!(dets.len(), 2);
        let drone = dets.iter().find(|d| d.class_label == "drone").unwrap();
        assert!((drone.confidence - 0.855).abs() < 1e-4);
    }

    #[test]
    fn nms_collapses_overlapping_same_class() {
        // Two near-identical drone boxes; NMS keeps the higher-confidence one.
        let l = labels();
        let rows: [[f32; 2]; 6] = [
            [100.0, 102.0],
            [100.0, 101.0],
            [40.0, 40.0],
            [40.0, 40.0],
            [0.90, 0.70],
            [0.01, 0.01],
        ];
        let mut data = Vec::new();
        for r in rows {
            data.extend_from_slice(&r);
        }
        let dets = decode(&data, 6, 2, &params(DetectionHead::Yolo8, &l, 0.25));
        assert_eq!(dets.len(), 1);
        assert!((dets[0].confidence - 0.90).abs() < 1e-4);
    }

    #[test]
    fn preprocess_lays_out_nchw_and_normalizes() {
        // A 2x2 RGB frame, resized to 2x2 (identity), normalized /255, NCHW.
        // Pixels: (0,0)=red(255,0,0) (1,0)=green(0,255,0)
        //         (0,1)=blue(0,0,255) (1,1)=white(255,255,255)
        let frame: Vec<u8> = vec![
            255, 0, 0, // (0,0) red
            0, 0, 255, // (0,1) blue
            0, 255, 0, // (1,0) green
            255, 255, 255, // (1,1) white
        ];
        let t = preprocess_rgb24_nchw(&frame, 2, 2, 2, 2).unwrap();
        assert_eq!(t.len(), 3 * 2 * 2);
        // R plane (channel 0): [r(0,0), r(0,1), r(1,0), r(1,1)] = [1,0,0,1].
        assert!((t[0] - 1.0).abs() < 1e-3); // (0,0) red R=1
        assert!((t[1] - 0.0).abs() < 1e-3); // (0,1) blue R=0
        assert!((t[3] - 1.0).abs() < 1e-3); // (1,1) white R=1
                                            // G plane (channel 1) starts at index 4.
        assert!((t[4 + 2] - 1.0).abs() < 1e-3); // (1,0) green G=1
    }

    #[test]
    fn preprocess_rejects_bad_dims_or_short_buffer() {
        assert!(preprocess_rgb24_nchw(&[0; 12], 0, 2, 2, 2).is_none());
        assert!(preprocess_rgb24_nchw(&[0; 3], 2, 2, 2, 2).is_none()); // too short
    }

    #[test]
    fn empty_or_undersized_decodes_to_nothing() {
        let l = labels();
        assert!(decode(&[], 0, 0, &params(DetectionHead::Yolo8, &l, 0.25)).is_empty());
        // A 2-row buffer cannot be a 4+nc head.
        assert!(decode(&[1.0, 2.0], 2, 1, &params(DetectionHead::Yolo8, &l, 0.25)).is_empty());
    }
}
