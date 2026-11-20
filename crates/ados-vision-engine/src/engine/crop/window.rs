//! The crop pass's frame geometry: the window it looks in, its detections
//! mapped back to frame pixels, and their merge with the whole frame's.

use ados_protocol::framebus::{BoundingBox, Detection};

use crate::engine::tracking::bbox_iou;

/// The height, in model-input pixels, the crop pass scales the designated
/// subject to: inside the range where a person detector run on the whole
/// frame finds a person reliably, with the rest of the window left as margin
/// for the subject's motion between batches.
pub(in crate::engine) const CROP_SUBJECT_MODEL_HEIGHT: f32 = 128.0;

/// The most the crop pass magnifies the frame (model pixels per frame pixel).
/// Past this the upsampled subject carries no more detail.
pub(in crate::engine) const CROP_MAX_UPSCALE: f32 = 4.0;

/// Two boxes over this IoU, one from each pass, are the same object.
pub(in crate::engine) const CROP_DUPLICATE_IOU: f32 = 0.4;

/// A crop detection whose edge lies within this many model pixels of a window
/// edge inside the frame is a subject cut by the window, not a whole one.
const CROP_EDGE_MARGIN: f32 = 2.0;

/// The size a coasting track's subject is assumed to keep per step without a
/// measurement. A coast holds the last measured box size, but a followed
/// subject the vehicle has stopped following (it holds while the lock is
/// unsure) walks away and shrinks: at 9 m and 1.4 m/s, about 5 % per 1/3 s.
/// Scaling the crop for the shrunken size keeps it magnifying the subject the
/// whole-frame pass has lost, and a subject that did not shrink only lands
/// larger in the model's input.
pub(in crate::engine) const CROP_COAST_SHRINK: f32 = 0.95;

/// The frame window the crop pass runs on: the model's input aspect, centred on
/// the predicted box, scaled so the box is `CROP_SUBJECT_MODEL_HEIGHT` tall in
/// the model's input (within `CROP_MAX_UPSCALE`, and no smaller than the frame
/// allows) after `coast_steps` steps of `CROP_COAST_SHRINK`, and moved inside
/// the frame. With `magnify_over`, the vertical scale of a whole-frame pass
/// running in the same batch, `None` when the window would not magnify the
/// subject beyond it (the subject is large and measured). `None` when the
/// inputs are degenerate.
pub(in crate::engine) fn crop_window(
    predicted: &BoundingBox,
    coast_steps: f32,
    frame_w: u32,
    frame_h: u32,
    input_w: u32,
    input_h: u32,
    magnify_over: Option<f32>,
) -> Option<BoundingBox> {
    if predicted.height < 1.0 || frame_w == 0 || frame_h == 0 || input_w == 0 || input_h == 0 {
        return None;
    }
    let (fw, fh, iw, ih) = (
        frame_w as f32,
        frame_h as f32,
        input_w as f32,
        input_h as f32,
    );
    // The smallest scale whose window still fits in the frame.
    let fit = (iw / fw).max(ih / fh);
    let expected_height = predicted.height * CROP_COAST_SHRINK.powf(coast_steps.clamp(0.0, 64.0));
    let scale = (CROP_SUBJECT_MODEL_HEIGHT / expected_height)
        .min(CROP_MAX_UPSCALE)
        .max(fit);
    if magnify_over.is_some_and(|whole_frame| scale <= whole_frame * 1.05) {
        return None;
    }
    let (width, height) = ((iw / scale).min(fw), (ih / scale).min(fh));
    let cx = predicted.x + predicted.width / 2.0;
    let cy = predicted.y + predicted.height / 2.0;
    Some(BoundingBox {
        x: (cx - width / 2.0).clamp(0.0, fw - width),
        y: (cy - height / 2.0).clamp(0.0, fh - height),
        width,
        height,
    })
}

/// Map the crop pass's detections (in the `input_w` x `input_h` crop) back to
/// frame pixels, keeping only whole subjects of `class`: a box cut by a window
/// edge inside the frame is dropped, and another class adds nothing the
/// whole-frame pass does not already report.
pub(in crate::engine) fn crop_to_frame(
    detections: Vec<Detection>,
    window: &BoundingBox,
    frame_w: u32,
    frame_h: u32,
    input_w: u32,
    input_h: u32,
    class: &str,
) -> Vec<Detection> {
    let sx = window.width / input_w as f32;
    let sy = window.height / input_h as f32;
    let inner_left = window.x > 0.5;
    let inner_top = window.y > 0.5;
    let inner_right = window.x + window.width < frame_w as f32 - 0.5;
    let inner_bottom = window.y + window.height < frame_h as f32 - 0.5;
    let (iw, ih) = (input_w as f32, input_h as f32);
    detections
        .into_iter()
        .filter(|d| d.class_label == class)
        .filter_map(|mut d| {
            let b = d.bbox?;
            let cut = (inner_left && b.x < CROP_EDGE_MARGIN)
                || (inner_top && b.y < CROP_EDGE_MARGIN)
                || (inner_right && b.x + b.width > iw - CROP_EDGE_MARGIN)
                || (inner_bottom && b.y + b.height > ih - CROP_EDGE_MARGIN);
            if cut {
                return None;
            }
            d.bbox = Some(BoundingBox {
                x: window.x + b.x * sx,
                y: window.y + b.y * sy,
                width: b.width * sx,
                height: b.height * sy,
            });
            Some(d)
        })
        .collect()
}

/// Add the crop pass's frame-space detections to the whole-frame ones. A crop
/// box that overlaps a whole-frame box is the same object: the more confident
/// of the two stays, so the tracker never sees one subject as two candidates.
pub(in crate::engine) fn merge_crop(detections: &mut Vec<Detection>, crop: Vec<Detection>) {
    for c in crop {
        let Some(cb) = c.bbox.as_ref() else { continue };
        let same = detections.iter().position(|d| {
            d.class_label == c.class_label
                && d.bbox
                    .as_ref()
                    .is_some_and(|db| bbox_iou(db, cb) > CROP_DUPLICATE_IOU)
        });
        match same {
            Some(i) if detections[i].confidence < c.confidence => detections[i] = c,
            Some(_) => {}
            None => detections.push(c),
        }
    }
}
