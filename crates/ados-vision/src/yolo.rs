//! YOLO detection-head decoding.
//!
//! Turns a model's raw output tensor into [`Detection`]s in source-frame pixels,
//! selecting the layout by [`DetectionHead`].
//!
//! * [`DetectionHead::Yolo8`], the transposed `[1, 4+nc, anchors]` head: four
//!   box rows (cx, cy, w, h) then one score row per class, no objectness. This
//!   is the ultralytics YOLOv8/v11 export.
//! * [`DetectionHead::Yolo5`], the legacy `[1, anchors, 5+nc]` head: per anchor
//!   `[cx, cy, w, h, objectness, class_scores...]` (YOLOv5/v7).
//! * [`DetectionHead::Yolox`], one undecoded grid per stride, is a multi-output
//!   layout this single-tensor decoder does not read: it decodes to no
//!   detections. The NPU sidecars decode it.
//!
//! The decoder is orientation robust: the raw output is a flat row-major buffer
//! with a logical 2D shape `(rows, cols)` (after the batch dim is dropped), and
//! the decoder transposes a head delivered as `(anchors, features)`
//! automatically. Boxes are in the model's input resolution: the input is the
//! source image letterboxed ([`Letterbox`]: resized with its aspect kept and
//! centred on a grey pad, as the YOLO families are trained), so a box maps
//! back through the letterbox's scale and pad to the source image, then to the
//! output pixels the caller asks for. Survivors of the confidence gate pass
//! class-agnostic non-maximum suppression.

use ados_protocol::framebus::{BoundingBox, Detection, DetectionHead};

/// The pad value of a letterboxed input, in `[0, 1]`: the grey (114 of 255)
/// the YOLO families pad their training images with.
pub const LETTERBOX_FILL: f32 = 114.0 / 255.0;

/// Where a `src_w` x `src_h` image sits in a letterboxed model input: scaled
/// by `scale` on both axes (the largest scale that fits the input) and
/// centred, `pad_x` / `pad_y` input pixels in from the left and top edges.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Letterbox {
    pub scale: f32,
    pub pad_x: f32,
    pub pad_y: f32,
}

impl Letterbox {
    /// The letterbox of a `src_w` x `src_h` image (both positive) in an
    /// `input_w` x `input_h` input.
    pub fn fit(src_w: f32, src_h: f32, input_w: u32, input_h: u32) -> Self {
        let (iw, ih) = (input_w as f32, input_h as f32);
        let scale = (iw / src_w).min(ih / src_h);
        Self {
            scale,
            pad_x: (iw - src_w * scale) / 2.0,
            pad_y: (ih - src_h * scale) / 2.0,
        }
    }

    /// A box in source-image pixels, in input pixels.
    pub fn to_input(&self, b: &BoundingBox) -> BoundingBox {
        BoundingBox {
            x: b.x * self.scale + self.pad_x,
            y: b.y * self.scale + self.pad_y,
            width: b.width * self.scale,
            height: b.height * self.scale,
        }
    }

    /// A box in input pixels, in source-image pixels.
    pub fn to_source(&self, b: &BoundingBox) -> BoundingBox {
        BoundingBox {
            x: (b.x - self.pad_x) / self.scale,
            y: (b.y - self.pad_y) / self.scale,
            width: b.width / self.scale,
            height: b.height / self.scale,
        }
    }
}

/// `window` (frame pixels) clamped to a `frame_w` x `frame_h` frame, as
/// `(x, y, width, height)`; `None` is the whole frame. `None` back when the
/// clamped window holds less than a pixel.
pub fn window_region(
    frame_w: u32,
    frame_h: u32,
    window: Option<&BoundingBox>,
) -> Option<(f32, f32, f32, f32)> {
    let (fw, fh) = (frame_w as f32, frame_h as f32);
    let Some(b) = window else {
        return Some((0.0, 0.0, fw, fh));
    };
    let x0 = b.x.clamp(0.0, fw);
    let y0 = b.y.clamp(0.0, fh);
    let x1 = (b.x + b.width).clamp(0.0, fw);
    let y1 = (b.y + b.height).clamp(0.0, fh);
    (x1 - x0 >= 1.0 && y1 - y0 >= 1.0).then_some((x0, y0, x1 - x0, y1 - y0))
}

/// Parameters for a single decode pass.
pub struct DecodeParams<'a> {
    pub head: DetectionHead,
    /// Class labels in output-index order. Empty ⇒ one unnamed class.
    pub labels: &'a [String],
    /// The model's input resolution (boxes are in these pixels).
    pub input_w: u32,
    pub input_h: u32,
    /// The image the input was letterboxed from ([`window_region`]: the
    /// frame, or the window of it), in its own pixels.
    pub src_w: f32,
    pub src_h: f32,
    /// The pixels the boxes come back in: the source image scaled to
    /// `frame_w` x `frame_h` (the source itself when they are its size).
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
        DetectionHead::Yolox => return Vec::new(),
    };
    if cands.is_empty() {
        return Vec::new();
    }

    if p.input_w == 0 || p.input_h == 0 || p.src_w <= 0.0 || p.src_h <= 0.0 {
        return Vec::new();
    }
    let lb = Letterbox::fit(p.src_w, p.src_h, p.input_w, p.input_h);
    let (out_x, out_y) = (p.frame_w as f32 / p.src_w, p.frame_h as f32 / p.src_h);

    // Corner-form boxes in output pixels, paired with score/class.
    let boxes: Vec<(BoundingBox, f32, usize)> = cands
        .into_iter()
        .map(|c| {
            let src = lb.to_source(&BoundingBox {
                x: c.cx - c.w / 2.0,
                y: c.cy - c.h / 2.0,
                width: c.w,
                height: c.h,
            });
            (
                BoundingBox {
                    x: src.x * out_x,
                    y: src.y * out_y,
                    width: src.width * out_x,
                    height: src.height * out_y,
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
            track_state: None,
            appearance_id: None,
        });
    }
    out
}

/// Preprocess a raw RGB24 frame into a model input tensor in `out`: letterbox
/// `window` (frame pixels, clamped to the frame by [`window_region`]; `None` is
/// the whole frame) into `(input_w, input_h)` ([`Letterbox`]: bilinear resize
/// with the aspect kept, centred, the rest [`LETTERBOX_FILL`]), scale to
/// `[0, 1]`, and lay it out channel-planar NCHW (`[3, input_h, input_w]`
/// flattened, R plane then G then B). `out` is resized to the tensor's length
/// only when it differs, so a buffer kept across frames is filled in place
/// with no allocation. Returns `false` (leaving `out` unspecified) when a
/// dimension is zero, the frame buffer is too small or the window holds less
/// than a pixel.
///
/// [`decode`] maps boxes back through the same letterbox (its `src_w` /
/// `src_h` are the window's size). A stretched 16:9 frame squeezed a person to
/// half their width in a square input, and the detector missed them. A window
/// with the input's aspect is sampled exactly as cutting it out with
/// [`crate::reid::crop_resize_rgb24`] at the input size would, without the
/// intermediate 8-bit image.
pub fn preprocess_rgb24_nchw_into(
    frame: &[u8],
    frame_w: u32,
    frame_h: u32,
    window: Option<&BoundingBox>,
    input_w: u32,
    input_h: u32,
    out: &mut Vec<f32>,
) -> bool {
    let (fw, fh, iw, ih) = (
        frame_w as usize,
        frame_h as usize,
        input_w as usize,
        input_h as usize,
    );
    if fw == 0 || fh == 0 || iw == 0 || ih == 0 || frame.len() < fw * fh * 3 {
        return false;
    }
    let Some((x0, y0, sw, sh)) = window_region(frame_w, frame_h, window) else {
        return false;
    };
    let lb = Letterbox::fit(sw, sh, input_w, input_h);
    // The input pixels the image covers; the rest is pad.
    let (c0, c1) = (
        (lb.pad_x.round() as usize).min(iw),
        ((lb.pad_x + sw * lb.scale).round() as usize).min(iw),
    );
    let (r0, r1) = (
        (lb.pad_y.round() as usize).min(ih),
        ((lb.pad_y + sh * lb.scale).round() as usize).min(ih),
    );
    let plane = ih * iw;
    if out.len() != 3 * plane {
        out.resize(3 * plane, 0.0);
    }
    let (r_plane, rest) = out.split_at_mut(plane);
    let (g_plane, b_plane) = rest.split_at_mut(plane);
    let mut pad = |from: usize, to: usize| {
        r_plane[from..to].fill(LETTERBOX_FILL);
        g_plane[from..to].fill(LETTERBOX_FILL);
        b_plane[from..to].fill(LETTERBOX_FILL);
    };
    pad(0, r0 * iw);
    pad(r1 * iw, plane);
    for oy in r0..r1 {
        pad(oy * iw, oy * iw + c0);
        pad(oy * iw + c1, (oy + 1) * iw);
    }
    let stride = fw * 3;
    let (max_x, max_y) = ((fw - 1) as f32, (fh - 1) as f32);
    let inv_scale = 1.0 / lb.scale;
    const INV_255: f32 = 1.0 / 255.0;
    for oy in r0..r1 {
        let fy = (y0 + (oy as f32 + 0.5 - lb.pad_y) * inv_scale - 0.5).clamp(0.0, max_y);
        let ylo = fy as usize;
        let yhi = (ylo + 1).min(fh - 1);
        let wy = fy - ylo as f32;
        let top = &frame[ylo * stride..ylo * stride + stride];
        let bot = &frame[yhi * stride..yhi * stride + stride];
        let row = oy * iw;
        for ox in c0..c1 {
            let fx = (x0 + (ox as f32 + 0.5 - lb.pad_x) * inv_scale - 0.5).clamp(0.0, max_x);
            let xlo = fx as usize;
            let xhi = (xlo + 1).min(fw - 1);
            let wx = fx - xlo as f32;
            let (a, b) = (xlo * 3, xhi * 3);
            let (w00, w01) = ((1.0 - wx) * (1.0 - wy), wx * (1.0 - wy));
            let (w10, w11) = ((1.0 - wx) * wy, wx * wy);
            let px = |c: usize| {
                (f32::from(top[a + c]) * w00
                    + f32::from(top[b + c]) * w01
                    + f32::from(bot[a + c]) * w10
                    + f32::from(bot[b + c]) * w11)
                    * INV_255
            };
            r_plane[row + ox] = px(0);
            g_plane[row + ox] = px(1);
            b_plane[row + ox] = px(2);
        }
    }
    true
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
            src_w: 640.0,
            src_h: 640.0,
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
        let mut t = Vec::new();
        assert!(preprocess_rgb24_nchw_into(&frame, 2, 2, None, 2, 2, &mut t));
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
        let mut t = Vec::new();
        assert!(!preprocess_rgb24_nchw_into(
            &[0; 12], 0, 2, None, 2, 2, &mut t
        ));
        assert!(!preprocess_rgb24_nchw_into(
            &[0; 3], 2, 2, None, 2, 2, &mut t
        )); // too short
        let sliver = BoundingBox {
            x: 1.5,
            y: 0.0,
            width: 4.0,
            height: 2.0,
        };
        assert!(!preprocess_rgb24_nchw_into(
            &[0; 12],
            2,
            2,
            Some(&sliver),
            2,
            2,
            &mut t
        ));
    }

    #[test]
    fn a_window_is_sampled_as_its_crop_would_be_and_the_buffer_is_reused() {
        // A 64x48 gradient frame; a window with the input's aspect resized
        // into 32x32 (no pad) matches the 8-bit crop of the same window to
        // within the crop's rounding.
        let (fw, fh) = (64u32, 48u32);
        let frame: Vec<u8> = (0..fw * fh * 3)
            .map(|i| ((i * 7 + (i / 3) % 13) % 251) as u8)
            .collect();
        let window = BoundingBox {
            x: 10.3,
            y: 5.7,
            width: 30.0,
            height: 30.0,
        };
        let crop = crate::reid::crop_resize_rgb24(&frame, fw, fh, &window, 32, 32).unwrap();
        let mut from_crop = Vec::new();
        assert!(preprocess_rgb24_nchw_into(
            &crop,
            32,
            32,
            None,
            32,
            32,
            &mut from_crop
        ));
        let mut direct = Vec::with_capacity(3 * 32 * 32);
        let ptr = direct.as_ptr();
        assert!(preprocess_rgb24_nchw_into(
            &frame,
            fw,
            fh,
            Some(&window),
            32,
            32,
            &mut direct
        ));
        assert_eq!(
            direct.as_ptr(),
            ptr,
            "a buffer of the right size is filled in place"
        );
        let worst = from_crop
            .iter()
            .zip(&direct)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(worst <= 0.6 / 255.0, "worst difference {worst}");
    }

    #[test]
    fn empty_or_undersized_decodes_to_nothing() {
        let l = labels();
        assert!(decode(&[], 0, 0, &params(DetectionHead::Yolo8, &l, 0.25)).is_empty());
        // A 2-row buffer cannot be a 4+nc head.
        assert!(decode(&[1.0, 2.0], 2, 1, &params(DetectionHead::Yolo8, &l, 0.25)).is_empty());
    }

    #[test]
    fn a_letterbox_round_trips_a_box_for_16_9_and_2_1_frames() {
        let b = BoundingBox {
            x: 612.0,
            y: 321.0,
            width: 65.0,
            height: 124.0,
        };
        for (w, h, scale, pad_y) in [(1280.0, 720.0, 0.5, 140.0), (2000.0, 1000.0, 0.32, 160.0)] {
            let lb = Letterbox::fit(w, h, 640, 640);
            assert!((lb.scale - scale).abs() < 1e-6, "{w}x{h}: {lb:?}");
            assert!(
                lb.pad_x.abs() < 1e-4 && (lb.pad_y - pad_y).abs() < 1e-3,
                "{lb:?}"
            );
            let i = lb.to_input(&b);
            // The aspect is kept: a person keeps their shape in the input.
            assert!((i.width / i.height - b.width / b.height).abs() < 1e-5);
            let back = lb.to_source(&i);
            for (a, z) in [
                (back.x, b.x),
                (back.y, b.y),
                (back.width, b.width),
                (back.height, b.height),
            ] {
                assert!((a - z).abs() < 1e-3, "{w}x{h}: {back:?}");
            }
        }
        // A tall window pads left and right.
        let lb = Letterbox::fit(100.0, 200.0, 320, 320);
        assert!((lb.pad_x - 80.0).abs() < 1e-4 && lb.pad_y.abs() < 1e-4);
    }

    #[test]
    fn decode_maps_a_letterboxed_box_back_to_the_frame() {
        // A YOLOv5-layout candidate at the input position of a person box in
        // a 1280x720 frame letterboxed into 640x640 decodes to that box, in
        // frame pixels and, at half the size, in a 640x360 output.
        let l = vec!["person".to_string()];
        let truth = BoundingBox {
            x: 612.0,
            y: 321.0,
            width: 65.0,
            height: 124.0,
        };
        let i = Letterbox::fit(1280.0, 720.0, 640, 640).to_input(&truth);
        let data = [
            i.x + i.width / 2.0,
            i.y + i.height / 2.0,
            i.width,
            i.height,
            0.9,
            0.9,
        ];
        for (out_w, out_h, k) in [(1280, 720, 1.0), (640, 360, 0.5)] {
            let p = DecodeParams {
                head: DetectionHead::Yolo5,
                labels: &l,
                input_w: 640,
                input_h: 640,
                src_w: 1280.0,
                src_h: 720.0,
                frame_w: out_w,
                frame_h: out_h,
                conf_threshold: 0.25,
                nms_iou: 0.45,
            };
            let d = decode(&data, 1, 6, &p);
            let b = d[0].bbox.unwrap();
            assert!((b.x - truth.x * k).abs() < 1e-2, "{b:?}");
            assert!((b.y - truth.y * k).abs() < 1e-2, "{b:?}");
            assert!((b.height - truth.height * k).abs() < 1e-2, "{b:?}");
        }
    }

    #[test]
    fn preprocess_pads_a_wide_frame_with_the_letterbox_grey() {
        // A 4x2 white frame into a 4x4 input: rows 1-2 are the image, rows 0
        // and 3 the pad.
        let frame = vec![255u8; 4 * 2 * 3];
        let mut t = Vec::new();
        assert!(preprocess_rgb24_nchw_into(&frame, 4, 2, None, 4, 4, &mut t));
        for c in 0..3 {
            let plane = &t[c * 16..(c + 1) * 16];
            for (k, v) in plane.iter().enumerate() {
                let want = if (4..12).contains(&k) {
                    1.0
                } else {
                    LETTERBOX_FILL
                };
                assert!((v - want).abs() < 1e-6, "channel {c} pixel {k}: {v}");
            }
        }
    }
}
