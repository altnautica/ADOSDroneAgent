//! A second detector pass on a crop around the designated track.
//!
//! A detector run on the whole frame squeezes the frame into its input, and a
//! person that is small in the frame falls below the size the model finds
//! reliably. A follow subject walking away crosses that size within seconds of
//! the designation, so the lock coasts and is lost while the subject is still
//! in plain view. While an operator-designated track is live, the engine also
//! runs the detector on a window around the track's predicted box, sized so
//! the subject lands near `CROP_SUBJECT_MODEL_HEIGHT` in the model's input, and
//! feeds what it finds, in frame coordinates, to the tracker with the
//! whole-frame detections.
//!
//! The pass costs one more inference per detector batch, only while a
//! designated track is live and only when the window magnifies the subject
//! beyond the whole frame's own scale. It runs through the same accelerator
//! lease and priority as the whole-frame pass, so the detector's rate and
//! its priority over depth are unchanged.

use ados_protocol::framebus::BoundingBox;

use super::tracking::bbox_iou;
use super::*;

/// The height, in model-input pixels, the crop pass scales the designated
/// subject to: inside the range where a person detector run on the whole
/// frame finds a person reliably, with the rest of the window left as margin
/// for the subject's motion between batches.
pub(super) const CROP_SUBJECT_MODEL_HEIGHT: f32 = 128.0;

/// The most the crop pass magnifies the frame (model pixels per frame pixel).
/// Past this the upsampled subject carries no more detail.
pub(super) const CROP_MAX_UPSCALE: f32 = 4.0;

/// Two boxes over this IoU, one from each pass, are the same object.
pub(super) const CROP_DUPLICATE_IOU: f32 = 0.4;

/// A crop detection whose edge lies within this many model pixels of a window
/// edge inside the frame is a subject cut by the window, not a whole one.
const CROP_EDGE_MARGIN: f32 = 2.0;

/// The frame window the crop pass runs on: the model's input aspect, centred on
/// the predicted box, scaled so the box is `CROP_SUBJECT_MODEL_HEIGHT` tall in
/// the model's input (within `CROP_MAX_UPSCALE`), and moved inside the frame.
/// `None` when the window would not magnify the subject beyond the whole-frame
/// pass (the subject is already large) or the inputs are degenerate.
pub(super) fn crop_window(
    predicted: &BoundingBox,
    frame_w: u32,
    frame_h: u32,
    input_w: u32,
    input_h: u32,
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
    // The smallest scale whose window still fits in the frame, and the
    // whole-frame pass's own vertical scale.
    let fit = (iw / fw).max(ih / fh);
    let whole_frame = ih / fh;
    let scale = (CROP_SUBJECT_MODEL_HEIGHT / predicted.height)
        .min(CROP_MAX_UPSCALE)
        .max(fit);
    if scale <= whole_frame * 1.05 {
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
pub(super) fn crop_to_frame(
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
pub(super) fn merge_crop(detections: &mut Vec<Detection>, crop: Vec<Detection>) {
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

impl VisionEngine {
    /// The crop pass for one batch: when the camera's tracker holds a live
    /// designated track and a window around its predicted box magnifies it,
    /// run `model_id` on that window and return the subject-class detections
    /// in frame pixels. Empty when there is nothing to look for, the frame is
    /// not rgb24 (the crop is cut from rgb24 pixels, as the re-id crops are),
    /// or the model's input size is unknown.
    pub(super) async fn designated_crop_detections(
        &self,
        model_id: &str,
        desc: &FrameDescriptor,
        frame: &[u8],
    ) -> Result<Vec<Detection>> {
        if !self.tracker_enabled || desc.format != FrameFormat::Rgb24 {
            return Ok(Vec::new());
        }
        let Some((predicted, class)) = self
            .trackers
            .lock()
            .await
            .get(&desc.camera_id)
            .and_then(|t| t.designated_prediction())
            .map(|(b, c)| (b, c.to_string()))
        else {
            return Ok(Vec::new());
        };
        let Some((iw, ih)) = self
            .models
            .lock()
            .await
            .get(model_id)
            .map(|m| (m.meta.input_width, m.meta.input_height))
        else {
            return Ok(Vec::new());
        };
        let Some(window) = crop_window(&predicted, desc.width, desc.height, iw, ih) else {
            return Ok(Vec::new());
        };
        let Some(crop) =
            ados_vision::reid::crop_resize_rgb24(frame, desc.width, desc.height, &window, iw, ih)
        else {
            return Ok(Vec::new());
        };
        // Not recorded in the model's timing: the telemetry rate stays the
        // rate of published batches, and the latency the whole-frame pass's.
        let found = self
            .run_model_untimed(model_id, &crop, iw, ih, FrameFormat::Rgb24)
            .await?;
        let found = crop_to_frame(found, &window, desc.width, desc.height, iw, ih, &class);
        {
            let mut counts = self
                .crop_counts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let c = counts.entry(desc.camera_id.clone()).or_default();
            c.runs += 1;
            c.hits += u64::from(!found.is_empty());
        }
        Ok(found)
    }

    /// The crop passes `camera_id` has run and found its subject on.
    pub(super) fn crop_counts(&self, camera_id: &str) -> CropCounts {
        self.crop_counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(camera_id)
            .copied()
            .unwrap_or_default()
    }
}

/// One camera's crop-pass counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct CropCounts {
    /// Batches the crop pass ran on.
    pub(super) runs: u64,
    /// Of those, batches it found a box of the designated track's class on.
    pub(super) hits: u64,
}
