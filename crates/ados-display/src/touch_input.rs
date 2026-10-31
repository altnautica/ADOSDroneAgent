//! Touchscreen reader: raw evdev ABS samples -> rotated LCD pixels -> gestures.
//!
//! The native page UI renders the 480x320 panel but is blind without a touch
//! source. This module opens the resistive-touch evdev node (the ADS7846 on the
//! Waveshare 3.5" SPI LCD), drains its ABS_X / ABS_Y / BTN_TOUCH stream, maps
//! each contact point from the panel's raw ADC range into landscape LCD pixels
//! for the display rotation, and feeds the points through the host-portable
//! [`ados_hmi::touch::StrokeFsm`]. Completed strokes are classified into a
//! [`ados_hmi::touch::TouchGesture`] and handed to the render loop over an mpsc channel, where
//! the navigator turns them into tab switches and modal pushes/pops.
//!
//! The panel's resistive overlay reports in a fixed portrait frame: ABS_X runs
//! across the short physical edge and ABS_Y down the long edge. The on-screen
//! UI is landscape (480 wide, 320 tall). [`TouchTransform::map_to_lcd`] places a raw contact on
//! the landscape surface through a 2x3 affine transform so the navigator always
//! receives coordinates in the same frame the pages lay their hit zones out in.
//!
//! The affine comes from one of two sources, resolved once at reader start by
//! [`load_transform`]:
//!   * a per-rig calibration the operator captured with the touch wizard,
//!     persisted to [`ados_hid::sidecar::TOUCH_CALIB_PATH`] and fit against the
//!     panel's real raw ADC counts, or
//!   * when no calibration file is present, the rotation-aware baseline from
//!     [`ados_hmi::affine::identity_for`], which maps the canonical 12-bit raw
//!     range to the LCD bounds for the configured rotation.
//!
//! Only the node discovery + the async event drain are target-gated to Linux
//! (they touch evdev). The coordinate transform and the raw-range normalizer are
//! pure and unit-tested for every rotation, mirroring the `#[cfg(...)]` split the
//! `ados_hmi::touch` / `ados_hid::input` modules use.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ados_hmi::affine::{self, Affine};

/// Landscape LCD width in pixels — the on-screen frame the navigator routes in.
pub const LCD_W: i32 = crate::pages::PANEL_W as i32;
/// Landscape LCD height in pixels.
pub const LCD_H: i32 = crate::pages::PANEL_H as i32;

/// On-disk touch-calibration path. Re-exported from the host-portable sidecar
/// module so callers do not reach across crates for the constant.
pub const TOUCH_CALIB_PATH: &str = ados_hid::sidecar::TOUCH_CALIB_PATH;

/// The default touch node when discovery cannot name a better match. The
/// ADS7846 binds first on the SPI-LCD ground stations, so `event0` is the
/// pragmatic fallback rather than a hard requirement.
pub const DEFAULT_TOUCH_NODE: &str = "/dev/input/event0";

/// The raw ABS range read from one axis' `input_absinfo`. The driver clips
/// samples to `[min, max]`, so the normalizer does not have to defend the input,
/// but a degenerate `min == max` range (a driver that never published calibrated
/// bounds) is guarded so it cannot divide by zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AxisRange {
    pub min: i32,
    pub max: i32,
}

impl AxisRange {
    /// A sane default for a 12-bit resistive ADC when the driver does not
    /// publish bounds (matches the affine module's `RAW_MIN..RAW_MAX`).
    pub const fn adc_12bit() -> Self {
        AxisRange { min: 0, max: 4095 }
    }

    /// Normalize a raw sample to `0.0..=1.0`, clamped. A zero-width range maps
    /// everything to `0.0` rather than dividing by zero.
    pub fn norm(self, raw: i32) -> f64 {
        let span = (self.max - self.min) as f64;
        if span <= 0.0 {
            return 0.0;
        }
        (((raw - self.min) as f64) / span).clamp(0.0, 1.0)
    }
}

/// The resolved touch coordinate transform: a 2x3 affine plus how to feed raw
/// samples into it.
///
/// A per-rig calibration ([`TouchTransform::calibrated`]) is fit against the
/// panel's real raw ADC counts, so its matrix already absorbs the driver's true
/// range and gets the raw point applied directly. The rotation-aware fallback
/// ([`TouchTransform::fallback`]) assumes the canonical 12-bit range, so a raw
/// point is first normalized over the driver's published ranges into that
/// canonical span before the matrix places it — which keeps a driver that
/// reports a non-default range (e.g. `200..3900`) mapping its full sweep to the
/// LCD corners.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TouchTransform {
    affine: Affine,
    /// `Some((x_range, y_range))` for the fallback path, which renormalizes raw
    /// counts into the canonical range first; `None` for a loaded calibration,
    /// whose matrix was fit against raw counts directly.
    renormalize: Option<(AxisRange, AxisRange)>,
}

impl TouchTransform {
    /// A loaded per-rig calibration applied directly to raw ADC counts.
    pub fn calibrated(affine: Affine) -> Self {
        TouchTransform {
            affine,
            renormalize: None,
        }
    }

    /// The rotation-aware identity fallback. Raw counts are renormalized over the
    /// driver's published ranges into the canonical 12-bit span the
    /// [`identity_for`](affine::identity_for) matrix expects.
    pub fn fallback(rotation: i32, x_range: AxisRange, y_range: AxisRange) -> Self {
        TouchTransform {
            affine: affine::identity_for(rotation, (LCD_W, LCD_H)),
            renormalize: Some((x_range, y_range)),
        }
    }

    /// True when this transform is a per-rig calibration (fit against raw ADC
    /// counts), false when it is the rotation-aware identity fallback. The
    /// render loop reads this to decide whether to auto-launch the calibration
    /// wizard on a panel that has a touch chip but no saved calibration.
    pub fn is_calibrated(&self) -> bool {
        self.renormalize.is_none()
    }

    /// Map a raw `(x_raw, y_raw)` ADC contact to landscape LCD pixels, clamped
    /// to `[0, LCD_W)` x `[0, LCD_H)`.
    pub fn map_to_lcd(&self, x_raw: i32, y_raw: i32) -> (i32, i32) {
        let (rx, ry) = match self.renormalize {
            // Renormalize the driver range into the canonical 12-bit span the
            // identity matrix is built against (0..RAW_MAX), so a non-default
            // driver range still spans the full LCD.
            Some((x_range, y_range)) => {
                let nx = x_range.norm(x_raw);
                let ny = y_range.norm(y_raw);
                let span = (affine::RAW_MAX - affine::RAW_MIN) as f64;
                (
                    (affine::RAW_MIN as f64 + nx * span).round() as i32,
                    (affine::RAW_MIN as f64 + ny * span).round() as i32,
                )
            }
            // A loaded calibration was fit against raw counts directly.
            None => (x_raw, y_raw),
        };
        let (x, y) = self.affine.apply(rx, ry);
        (x.clamp(0, LCD_W - 1), y.clamp(0, LCD_H - 1))
    }
}

/// Resolve the touch transform for `rotation`: a per-rig calibration from
/// `calib_path` if one is present and valid, else the rotation-aware fallback
/// built from [`identity_for`](affine::identity_for) and the driver's ranges.
pub fn load_transform(
    calib_path: &std::path::Path,
    rotation: i32,
    x_range: AxisRange,
    y_range: AxisRange,
) -> TouchTransform {
    // load_for, not load: reject a calibration captured on a different panel
    // geometry or rotation so it falls back to the rotation-aware identity (and
    // the wizard re-prompts) rather than mis-mapping every touch on a swapped
    // panel.
    match affine::load_for(calib_path, (LCD_W, LCD_H), rotation) {
        Some(a) => TouchTransform::calibrated(a),
        None => TouchTransform::fallback(rotation, x_range, y_range),
    }
}

/// A completed touch stroke handed to the render loop: the classified gesture
/// (which the navigator routes) plus the raw ADC contact point at pen-up (which
/// the calibration wizard records against the on-screen target). The raw point
/// is carried alongside the gesture so the same channel serves both the normal
/// UI and calibration without a second reader.
#[derive(Debug, Clone)]
pub struct TouchEvent {
    pub gesture: ados_hmi::touch::TouchGesture,
    /// The raw, pre-transform `(x, y)` ADC sample at pen-up.
    pub raw: (i32, i32),
}

/// Per-handle transform state behind the mutex: the resolved transform plus the
/// inputs needed to rebuild it after a calibration save.
struct TransformState {
    transform: TouchTransform,
    rotation: i32,
    x_range: AxisRange,
    y_range: AxisRange,
}

struct HandleInner {
    state: Mutex<TransformState>,
    /// Set once the reader discovers a touch node, so the render loop can tell a
    /// panel with a touch chip (auto-prompt eligible) from a panel with none.
    present: AtomicBool,
}

/// A shared, reloadable touch transform.
///
/// The reader maps every contact through this handle, so swapping the inner
/// transform after a calibration save takes effect on the very next touch
/// without restarting the reader. The render loop holds a clone to call
/// [`TouchTransformHandle::reload`] when the wizard writes a new calibration and
/// to read [`TouchTransformHandle::touch_present`] /
/// [`TouchTransformHandle::is_calibrated`] for the auto-prompt gate.
#[derive(Clone)]
pub struct TouchTransformHandle {
    inner: Arc<HandleInner>,
}

impl TouchTransformHandle {
    /// Build a handle for `rotation`, seeded from any persisted calibration (or
    /// the rotation-aware fallback over the 12-bit ADC default until the reader
    /// learns the panel's real ranges via [`TouchTransformHandle::set_ranges`]).
    pub fn new(rotation: i32) -> Self {
        let x_range = AxisRange::adc_12bit();
        let y_range = AxisRange::adc_12bit();
        let transform = load_transform(
            std::path::Path::new(TOUCH_CALIB_PATH),
            rotation,
            x_range,
            y_range,
        );
        Self {
            inner: Arc::new(HandleInner {
                state: Mutex::new(TransformState {
                    transform,
                    rotation,
                    x_range,
                    y_range,
                }),
                present: AtomicBool::new(false),
            }),
        }
    }

    /// Mark a touch node present (the reader found a panel).
    pub fn mark_present(&self) {
        self.inner.present.store(true, Ordering::Relaxed);
    }

    /// Mark the touch node gone (absent at discovery, or lost mid-read), so the
    /// calibration auto-prompt does not fire for a panel that is not there.
    pub fn mark_absent(&self) {
        self.inner.present.store(false, Ordering::Relaxed);
    }

    /// Whether a touch node is currently open. False on a panel with no touch
    /// chip, and while the reader is rediscovering a lost one.
    pub fn touch_present(&self) -> bool {
        self.inner.present.load(Ordering::Relaxed)
    }

    /// Adopt the driver's real ABS ranges and re-resolve the transform against
    /// them. Called once by the reader after node discovery.
    pub fn set_ranges(&self, x_range: AxisRange, y_range: AxisRange) {
        let mut s = self.inner.state.lock().expect("touch transform mutex");
        s.x_range = x_range;
        s.y_range = y_range;
        s.transform = load_transform(
            std::path::Path::new(TOUCH_CALIB_PATH),
            s.rotation,
            x_range,
            y_range,
        );
    }

    /// Re-read the calibration file and swap the transform in place. Called by
    /// the render loop after the wizard saves a new calibration.
    pub fn reload(&self) {
        let mut s = self.inner.state.lock().expect("touch transform mutex");
        let (rotation, x_range, y_range) = (s.rotation, s.x_range, s.y_range);
        s.transform = load_transform(
            std::path::Path::new(TOUCH_CALIB_PATH),
            rotation,
            x_range,
            y_range,
        );
    }

    /// Map a raw ADC contact to LCD pixels through the current transform.
    pub fn map_to_lcd(&self, x_raw: i32, y_raw: i32) -> (i32, i32) {
        self.inner
            .state
            .lock()
            .expect("touch transform mutex")
            .transform
            .map_to_lcd(x_raw, y_raw)
    }

    /// Whether the current transform is a saved calibration (not the fallback).
    pub fn is_calibrated(&self) -> bool {
        self.inner
            .state
            .lock()
            .expect("touch transform mutex")
            .transform
            .is_calibrated()
    }
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::run_touch_reader;

#[cfg(test)]
mod tests;
