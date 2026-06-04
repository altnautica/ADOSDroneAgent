//! Touchscreen reader: raw evdev ABS samples -> rotated LCD pixels -> gestures.
//!
//! The native page UI renders the 480x320 panel but is blind without a touch
//! source. This module opens the resistive-touch evdev node (the ADS7846 on the
//! Waveshare 3.5" SPI LCD), drains its ABS_X / ABS_Y / BTN_TOUCH stream, maps
//! each contact point from the panel's raw ADC range into landscape LCD pixels
//! for the display rotation, and feeds the points through the host-portable
//! [`ados_hid::touch::StrokeFsm`]. Completed strokes are classified into a
//! [`TouchGesture`] and handed to the render loop over an mpsc channel, where
//! the navigator turns them into tab switches and modal pushes/pops.
//!
//! The panel's resistive overlay reports in a fixed portrait frame: ABS_X runs
//! across the short physical edge and ABS_Y down the long edge. The on-screen
//! UI is landscape (480 wide, 320 tall). [`map_to_lcd`] places a raw contact on
//! the landscape surface through a 2x3 affine transform so the navigator always
//! receives coordinates in the same frame the pages lay their hit zones out in.
//!
//! The affine comes from one of two sources, resolved once at reader start by
//! [`load_transform`]:
//!   * a per-rig calibration the operator captured with the touch wizard,
//!     persisted to [`ados_hid::sidecar::TOUCH_CALIB_PATH`] and fit against the
//!     panel's real raw ADC counts, or
//!   * when no calibration file is present, the rotation-aware baseline from
//!     [`ados_hid::affine::identity_for`], which maps the canonical 12-bit raw
//!     range to the LCD bounds for the configured rotation.
//!
//! Only the node discovery + the async event drain are target-gated to Linux
//! (they touch evdev). The coordinate transform and the raw-range normalizer are
//! pure and unit-tested for every rotation, mirroring the `#[cfg(...)]` split the
//! sibling `ados_hid::touch` / `ados_hid::input` modules use.

// `TouchGesture` is only named in the Linux-gated reader signature; importing it
// at module scope would be an unused import on non-Linux hosts where the reader
// is cfg'd out, so it is referenced by full path in the function below instead.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ados_hid::affine::{self, Affine};

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
    match affine::load(calib_path) {
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
    pub gesture: ados_hid::touch::TouchGesture,
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

    /// Whether a touch node was discovered. False on a panel with no touch chip.
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

/// Discover, open, and drain the touchscreen evdev node, classifying strokes and
/// sending each completed [`TouchEvent`] on `tx`. Runs until the device errors
/// or the channel closes (the render loop dropped its receiver on shutdown).
///
/// Discovery prefers a device whose name contains "ADS7846" or "Touchscreen",
/// then any device that advertises both ABS_X and ABS_Y and is not a gamepad,
/// and finally falls back to [`DEFAULT_TOUCH_NODE`]. The chosen node's published
/// ABS ranges seed the normalizer; a missing range falls back to the 12-bit ADC
/// default. The coordinate transform is owned by `handle` (shared with the
/// render loop so a fresh calibration reloads without restarting the reader):
/// the reader feeds the discovered ranges in via
/// [`TouchTransformHandle::set_ranges`] and maps every sample through it. Every
/// pen-up is logged at info level with the raw point, the mapped LCD point, and
/// the gesture kind so the mapping can be checked on the rig.
#[cfg(target_os = "linux")]
pub async fn run_touch_reader(
    handle: TouchTransformHandle,
    tx: tokio::sync::mpsc::Sender<TouchEvent>,
) -> std::io::Result<()> {
    use std::time::Instant;

    use ados_hid::touch::StrokeFsm;
    use evdev::{AbsoluteAxisType, InputEventKind, Key};

    let Some((path, device)) = discover_touch_device() else {
        tracing::info!("ados-display: no touchscreen evdev node found; touch input disabled");
        return Ok(());
    };

    let (x_range, y_range) = abs_ranges(&device);
    // Adopt the panel's real ABS ranges and mark the chip present so the render
    // loop's auto-prompt gate fires for an uncalibrated touch panel.
    handle.set_ranges(x_range, y_range);
    handle.mark_present();
    let calibrated = handle.is_calibrated();
    tracing::info!(
        path = %path,
        name = device.name().unwrap_or("unknown"),
        x_min = x_range.min,
        x_max = x_range.max,
        y_min = y_range.min,
        y_max = y_range.max,
        calibrated,
        "touchscreen opened; reading"
    );

    let mut stream = device.into_event_stream()?;
    let mut fsm = StrokeFsm::new();
    let base = Instant::now();

    // Raw ABS coordinates accumulate between SYN_REPORT markers and are applied
    // once per report so x and y move together (a half-updated point is never
    // fed to the FSM).
    let mut pending_x: Option<i32> = None;
    let mut pending_y: Option<i32> = None;
    // Last seen point, logged on pen-up alongside the gesture for calibration.
    let mut last_raw: (i32, i32) = (0, 0);
    let mut last_lcd: (i32, i32) = (0, 0);

    loop {
        let ev = match stream.next_event().await {
            Ok(ev) => ev,
            Err(e) => {
                tracing::warn!(error = %e, "touchscreen read error; stopping touch reader");
                return Err(e);
            }
        };
        let now_ms = base.elapsed().as_millis() as i64;
        match ev.kind() {
            InputEventKind::Key(Key::BTN_TOUCH) => {
                if ev.value() == 1 && !fsm.pen_down() {
                    fsm.open_stroke(now_ms);
                } else if ev.value() == 0 && fsm.pen_down() {
                    if let Some(gesture) = fsm.close_stroke(now_ms) {
                        tracing::info!(
                            raw_x = last_raw.0,
                            raw_y = last_raw.1,
                            lcd_x = last_lcd.0,
                            lcd_y = last_lcd.1,
                            gesture = gesture.kind.as_str(),
                            "touch pen-up"
                        );
                        // Carry the raw pen-up point so the calibration wizard
                        // can fit it against the on-screen target. A closed
                        // channel means the render loop is gone; stop.
                        let event = TouchEvent {
                            gesture,
                            raw: last_raw,
                        };
                        if tx.send(event).await.is_err() {
                            tracing::info!("touch gesture channel closed; stopping reader");
                            return Ok(());
                        }
                    }
                    pending_x = None;
                    pending_y = None;
                }
            }
            InputEventKind::AbsAxis(axis) => {
                if axis == AbsoluteAxisType::ABS_X {
                    pending_x = Some(ev.value());
                } else if axis == AbsoluteAxisType::ABS_Y {
                    pending_y = Some(ev.value());
                }
            }
            InputEventKind::Synchronization(_) if fsm.pen_down() => {
                if let (Some(x), Some(y)) = (pending_x, pending_y) {
                    let (x_lcd, y_lcd) = handle.map_to_lcd(x, y);
                    last_raw = (x, y);
                    last_lcd = (x_lcd, y_lcd);
                    fsm.record_move(x_lcd, y_lcd, now_ms);
                }
            }
            _ => {}
        }
    }
}

/// Find the touchscreen evdev node. Returns the opened device and its path, or
/// `None` when no candidate (and not even the fallback node) can be opened.
#[cfg(target_os = "linux")]
fn discover_touch_device() -> Option<(String, evdev::Device)> {
    let mut fallback: Option<(String, evdev::Device)> = None;

    for (path, device) in evdev::enumerate() {
        let path = path.to_string_lossy().to_string();
        let name = device.name().unwrap_or("").to_string();
        let name_matches = {
            let lower = name.to_ascii_lowercase();
            lower.contains("ads7846") || lower.contains("touchscreen")
        };
        if name_matches {
            return Some((path, device));
        }
        // A device with both ABS axes and few keys is a touch panel, not a
        // gamepad. Keep the first such device as a fallback in case no named
        // touchscreen turns up.
        if fallback.is_none() && is_touch_like(&device) {
            fallback = Some((path, device));
        }
    }

    if let Some(found) = fallback {
        return Some(found);
    }

    // Nothing in the enumeration matched; try the conventional node directly so
    // a driver that does not advertise a usable name still works.
    match evdev::Device::open(DEFAULT_TOUCH_NODE) {
        Ok(dev) => Some((DEFAULT_TOUCH_NODE.to_string(), dev)),
        Err(_) => None,
    }
}

/// True when a device looks like a single-touch panel: it has both ABS_X and
/// ABS_Y and a small key set (a touchscreen has BTN_TOUCH; a gamepad has a wide
/// button bank). Mirrors the structural test the gamepad predicate uses, but
/// inverted — a touch panel is the low-button-count ABS device.
#[cfg(target_os = "linux")]
fn is_touch_like(device: &evdev::Device) -> bool {
    use evdev::AbsoluteAxisType;
    let has_abs = device
        .supported_absolute_axes()
        .map(|set| set.contains(AbsoluteAxisType::ABS_X) && set.contains(AbsoluteAxisType::ABS_Y))
        .unwrap_or(false);
    if !has_abs {
        return false;
    }
    let key_count = device
        .supported_keys()
        .map(|k| k.iter().count())
        .unwrap_or(0);
    // A touchscreen reports a handful of keys (BTN_TOUCH, maybe BTN_TOOL_*); a
    // gamepad reports many. Stay well under the gamepad floor.
    key_count < ados_hid::input::MIN_GAMEPAD_BUTTONS
}

/// Read the ABS_X / ABS_Y ranges the touch driver published, falling back to the
/// 12-bit ADC default when an axis has no usable bounds (`min == max`).
#[cfg(target_os = "linux")]
fn abs_ranges(device: &evdev::Device) -> (AxisRange, AxisRange) {
    use evdev::AbsoluteAxisType;

    let states = device.get_abs_state().ok();
    let pick = |axis: AbsoluteAxisType| -> AxisRange {
        let Some(states) = states.as_ref() else {
            return AxisRange::adc_12bit();
        };
        let info = states[axis.0 as usize];
        if info.maximum > info.minimum {
            AxisRange {
                min: info.minimum,
                max: info.maximum,
            }
        } else {
            AxisRange::adc_12bit()
        }
    };
    (pick(AbsoluteAxisType::ABS_X), pick(AbsoluteAxisType::ABS_Y))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The full-range portrait corners for clarity in the rotation tests.
    const X: AxisRange = AxisRange { min: 0, max: 4095 };
    const Y: AxisRange = AxisRange { min: 0, max: 4095 };

    // The rotation-aware fallback transform applied to a raw corner. Full-range
    // ranges make the renormalization a no-op, so this exercises the bare
    // `identity_for` matrix for each rotation.
    fn corner(x_raw: i32, y_raw: i32, rotation: i32) -> (i32, i32) {
        TouchTransform::fallback(rotation, X, Y).map_to_lcd(x_raw, y_raw)
    }

    #[test]
    fn norm_clamps_and_handles_degenerate_range() {
        let r = AxisRange { min: 100, max: 200 };
        assert_eq!(r.norm(100), 0.0);
        assert_eq!(r.norm(200), 1.0);
        assert_eq!(r.norm(150), 0.5);
        // Below/above the range clamp to the ends.
        assert_eq!(r.norm(50), 0.0);
        assert_eq!(r.norm(500), 1.0);
        // Degenerate (min == max) never divides by zero.
        let z = AxisRange { min: 7, max: 7 };
        assert_eq!(z.norm(7), 0.0);
        assert_eq!(z.norm(999), 0.0);
    }

    #[test]
    fn rotation_0_is_a_straight_scale() {
        // identity_for(0): x = sx*x_raw, y = sy*y_raw. The far corners scale to
        // exactly LCD_W / LCD_H, which the clamp pins to the last valid pixel.
        assert_eq!(corner(0, 0, 0), (0, 0));
        assert_eq!(corner(4095, 0, 0), (LCD_W - 1, 0));
        assert_eq!(corner(0, 4095, 0), (0, LCD_H - 1));
        assert_eq!(corner(4095, 4095, 0), (LCD_W - 1, LCD_H - 1));
        // centre: sx*2047=239.88 -> 240; sy*2047=159.92 -> 160.
        assert_eq!(corner(2047, 2047, 0), (240, 160));
    }

    #[test]
    fn rotation_90_swaps_axes_and_flips_y() {
        // identity_for(90): x = (LCD_W/span)*y_raw, y = LCD_H - (LCD_H/span)*x_raw.
        assert_eq!(corner(0, 0, 90), (0, LCD_H - 1));
        assert_eq!(corner(4095, 0, 90), (0, 0));
        assert_eq!(corner(0, 4095, 90), (LCD_W - 1, LCD_H - 1));
        assert_eq!(corner(4095, 4095, 90), (LCD_W - 1, 0));
        // centre: x=(480/span)*2047=239.88 -> 240; y=320-159.92=160.08 -> 160.
        assert_eq!(corner(2047, 2047, 90), (240, 160));
    }

    #[test]
    fn rotation_180_inverts_both_axes() {
        // identity_for(180): x = LCD_W - sx*x_raw, y = LCD_H - sy*y_raw.
        assert_eq!(corner(0, 0, 180), (LCD_W - 1, LCD_H - 1));
        assert_eq!(corner(4095, 0, 180), (0, LCD_H - 1));
        assert_eq!(corner(0, 4095, 180), (LCD_W - 1, 0));
        assert_eq!(corner(4095, 4095, 180), (0, 0));
        // centre: both axes flipped -> 480-239.88=240.12 -> 240, 320-159.92=160.08 -> 160.
        assert_eq!(corner(2047, 2047, 180), (240, 160));
    }

    #[test]
    fn rotation_270_swaps_axes_and_flips_x() {
        // identity_for(270): x = LCD_W - (LCD_W/span)*y_raw, y = (LCD_H/span)*x_raw.
        assert_eq!(corner(0, 0, 270), (LCD_W - 1, 0));
        assert_eq!(corner(4095, 0, 270), (LCD_W - 1, LCD_H - 1));
        assert_eq!(corner(0, 4095, 270), (0, 0));
        assert_eq!(corner(4095, 4095, 270), (0, LCD_H - 1));
        // centre: x=480-239.88=240.12 -> 240; y=159.92 -> 160.
        assert_eq!(corner(2047, 2047, 270), (240, 160));
    }

    #[test]
    fn out_of_range_rotation_falls_back_to_zero() {
        // identity_for() collapses any rotation that is not a cardinal multiple
        // of 90 (after rem_euclid(360)) to rotation 0. (360 -> 0; 1000 -> 280;
        // -315 -> 45; 137 stays 137 — all non-cardinal -> rotation 0.)
        for bad in [45, 137, 280, 360, 1000, -315] {
            assert_eq!(corner(0, 0, bad), corner(0, 0, 0));
            assert_eq!(corner(4095, 4095, bad), corner(4095, 4095, 0));
            assert_eq!(corner(1234, 2345, bad), corner(1234, 2345, 0));
        }
    }

    #[test]
    fn negative_270_equivalent_to_90() {
        // rem_euclid normalizes -270 to 90.
        assert_eq!(corner(1000, 2000, -270), corner(1000, 2000, 90));
    }

    #[test]
    fn output_is_always_in_bounds() {
        // Sweep a grid across the raw range for every rotation; nothing escapes
        // the landscape surface, including the max corner (which the clamp pins
        // to W-1 / H-1 rather than W / H).
        for rot in [0, 90, 180, 270] {
            let t = TouchTransform::fallback(rot, X, Y);
            for &xr in &[0, 1, 2047, 4094, 4095, 5000] {
                for &yr in &[0, 1, 2047, 4094, 4095, 5000] {
                    let (x, y) = t.map_to_lcd(xr, yr);
                    assert!((0..LCD_W).contains(&x), "x={x} rot={rot}");
                    assert!((0..LCD_H).contains(&y), "y={y} rot={rot}");
                }
            }
        }
    }

    #[test]
    fn non_full_ranges_normalize_before_placing() {
        // A driver that reports 200..3900 still maps its own min/max to the LCD
        // corners: the fallback renormalizes the driver span into the canonical
        // range before the identity matrix places it.
        let xr = AxisRange {
            min: 200,
            max: 3900,
        };
        let yr = AxisRange {
            min: 150,
            max: 3950,
        };
        let t = TouchTransform::fallback(0, xr, yr);
        // Rotation-0 straight scale: (min,min) -> origin, (max,max) -> far corner.
        assert_eq!(t.map_to_lcd(200, 150), (0, 0));
        assert_eq!(t.map_to_lcd(3900, 3950), (LCD_W - 1, LCD_H - 1));
    }

    #[test]
    fn calibrated_transform_applies_matrix_to_raw_directly() {
        // A loaded calibration is applied to raw counts without renormalizing.
        // A simple scale-only affine (a=sx, e=sy) maps the raw corners to the
        // LCD bounds, which the clamp pins to the last valid pixel.
        let m = affine::identity_for(0, (LCD_W, LCD_H));
        let t = TouchTransform::calibrated(m);
        assert_eq!(t.map_to_lcd(0, 0), (0, 0));
        assert_eq!(t.map_to_lcd(4095, 4095), (LCD_W - 1, LCD_H - 1));
        // The driver ranges are ignored on this path: the same raw point lands
        // in the same place regardless of what the panel published.
        assert_eq!(t.map_to_lcd(2047, 2047), (240, 160));
    }

    #[test]
    fn load_transform_falls_back_when_no_calib_file() {
        // No file at the path -> the rotation-aware identity fallback.
        let t = load_transform(std::path::Path::new("/nonexistent/touch.calib"), 0, X, Y);
        assert_eq!(t, TouchTransform::fallback(0, X, Y));
    }
}
