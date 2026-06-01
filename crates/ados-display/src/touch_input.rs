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
//! UI is landscape (480 wide, 320 tall). [`map_to_lcd`] collapses both the
//! portrait->landscape swap and the panel rotation into one normalize-then-place
//! step so the navigator always receives coordinates in the same frame the
//! pages lay their hit zones out in.
//!
//! Only the node discovery + the async event drain are target-gated to Linux
//! (they touch evdev). The coordinate transform and the raw-range normalizer are
//! pure and unit-tested for every rotation, mirroring the `#[cfg(...)]` split the
//! sibling `ados_hid::touch` / `ados_hid::input` modules use.

// `TouchGesture` is only named in the Linux-gated reader signature; importing it
// at module scope would be an unused import on non-Linux hosts where the reader
// is cfg'd out, so it is referenced by full path in the function below instead.

/// Landscape LCD width in pixels — the on-screen frame the navigator routes in.
pub const LCD_W: i32 = crate::pages::PANEL_W as i32;
/// Landscape LCD height in pixels.
pub const LCD_H: i32 = crate::pages::PANEL_H as i32;

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

/// Map a raw `(x_raw, y_raw)` ADC contact to landscape LCD pixels for the panel
/// `rotation` (0 / 90 / 180 / 270; any other value is treated as 0).
///
/// The two raw axes are first normalized to `0.0..=1.0` over their published
/// ranges, giving `(nx, ny)` (the raw short and long axes). The rotation then
/// places that normalized point on the landscape 480x320 surface. The rotation-0
/// mapping is calibrated against the Waveshare 3.5" ADS7846 panel: on-screen X
/// comes from the long axis inverted (`1 - ny`) and on-screen Y from the short
/// axis (`nx`). The result is clamped to `[0, LCD_W)` x `[0, LCD_H)`.
pub fn map_to_lcd(
    x_raw: i32,
    y_raw: i32,
    x_range: AxisRange,
    y_range: AxisRange,
    rotation: i32,
) -> (i32, i32) {
    let nx = x_range.norm(x_raw); // short panel edge, 0..1
    let ny = y_range.norm(y_raw); // long panel edge, 0..1
    let rotation = rotation.rem_euclid(360);

    // Place the normalized point onto the landscape surface. `fx`/`fy` are
    // normalized landscape coordinates (0..1) before scaling to pixels. The
    // rotation-0 case is hardware-calibrated against the Waveshare 3.5" ADS7846
    // ground-station panel: on-screen X is the raw long axis inverted (1 - ny),
    // on-screen Y is the raw short axis (nx). The other cardinal rotations are
    // derived placeholders pending per-orientation hardware calibration.
    let (fx, fy) = match rotation {
        90 => (nx, 1.0 - ny),
        180 => (1.0 - ny, 1.0 - nx),
        270 => (1.0 - nx, ny),
        // 0 (and any out-of-range value normalized above). Hardware-calibrated.
        _ => (1.0 - ny, nx),
    };

    let x = (fx * LCD_W as f64) as i32;
    let y = (fy * LCD_H as f64) as i32;
    (x.clamp(0, LCD_W - 1), y.clamp(0, LCD_H - 1))
}

/// Discover, open, and drain the touchscreen evdev node, classifying strokes and
/// sending each completed [`TouchGesture`] on `tx`. Runs until the device errors
/// or the channel closes (the render loop dropped its receiver on shutdown).
///
/// Discovery prefers a device whose name contains "ADS7846" or "Touchscreen",
/// then any device that advertises both ABS_X and ABS_Y and is not a gamepad,
/// and finally falls back to [`DEFAULT_TOUCH_NODE`]. The chosen node's published
/// ABS ranges seed the normalizer; a missing range falls back to the 12-bit ADC
/// default. Every pen-up is logged at info level with the raw point, the mapped
/// LCD point, and the gesture kind so the mapping can be calibrated on the rig.
#[cfg(target_os = "linux")]
pub async fn run_touch_reader(
    rotation: i32,
    tx: tokio::sync::mpsc::Sender<ados_hid::touch::TouchGesture>,
) -> std::io::Result<()> {
    use std::time::Instant;

    use ados_hid::touch::StrokeFsm;
    use evdev::{AbsoluteAxisType, InputEventKind, Key};

    let Some((path, device)) = discover_touch_device() else {
        tracing::info!("ados-display: no touchscreen evdev node found; touch input disabled");
        return Ok(());
    };

    let (x_range, y_range) = abs_ranges(&device);
    tracing::info!(
        path = %path,
        name = device.name().unwrap_or("unknown"),
        x_min = x_range.min,
        x_max = x_range.max,
        y_min = y_range.min,
        y_max = y_range.max,
        rotation,
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
                        // A closed channel means the render loop is gone; stop.
                        if tx.send(gesture).await.is_err() {
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
                    let (x_lcd, y_lcd) = map_to_lcd(x, y, x_range, y_range, rotation);
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

    fn corner(x_raw: i32, y_raw: i32, rotation: i32) -> (i32, i32) {
        map_to_lcd(x_raw, y_raw, X, Y, rotation)
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
    fn rotation_0_calibrated_x_inverts_long_axis_y_is_short_axis() {
        // Hardware-calibrated rotation-0: fx = 1 - ny, fy = nx.
        // raw (0,0): nx=0, ny=0 -> fx=1, fy=0 -> top-right.
        assert_eq!(corner(0, 0, 0), (LCD_W - 1, 0));
        // raw (max, 0): nx=1, ny=0 -> fx=1, fy=1 -> bottom-right.
        assert_eq!(corner(4095, 0, 0), (LCD_W - 1, LCD_H - 1));
        // raw (0, max): nx=0, ny=1 -> fx=0, fy=0 -> top-left.
        assert_eq!(corner(0, 4095, 0), (0, 0));
        // raw (max, max): nx=1, ny=1 -> fx=0, fy=1 -> bottom-left.
        assert_eq!(corner(4095, 4095, 0), (0, LCD_H - 1));
        // centre: fx=1-0.49988=0.50012 -> 0.50012*480=240.0 -> 240;
        // fy=0.49988 -> 0.49988*320=159.9 -> 159.
        assert_eq!(corner(2047, 2047, 0), (240, 159));
    }

    #[test]
    fn rotation_90_swaps_and_flips_y() {
        // fx = nx, fy = 1 - ny.
        // raw (0,0): nx=0, ny=0 -> fx=0, fy=1 -> bottom-left.
        assert_eq!(corner(0, 0, 90), (0, LCD_H - 1));
        // raw (max,0): nx=1, ny=0 -> fx=1, fy=1 -> bottom-right.
        assert_eq!(corner(4095, 0, 90), (LCD_W - 1, LCD_H - 1));
        // raw (0,max): nx=0, ny=1 -> fx=0, fy=0 -> top-left.
        assert_eq!(corner(0, 4095, 90), (0, 0));
        // raw (max,max): nx=1, ny=1 -> fx=1, fy=0 -> top-right.
        assert_eq!(corner(4095, 4095, 90), (LCD_W - 1, 0));
        // centre: fx=nx=0.49988 -> 239; fy=1-ny=0.50012 -> 0.50012*320=160.0 -> 160.
        assert_eq!(corner(2047, 2047, 90), (239, 160));
    }

    #[test]
    fn rotation_180_inverts_rotation_0() {
        // fx = 1 - ny, fy = 1 - nx.
        // raw (0,0): nx=0, ny=0 -> fx=1, fy=1 -> bottom-right.
        assert_eq!(corner(0, 0, 180), (LCD_W - 1, LCD_H - 1));
        // raw (max,0): nx=1, ny=0 -> fx=1, fy=0 -> top-right.
        assert_eq!(corner(4095, 0, 180), (LCD_W - 1, 0));
        // raw (0,max): nx=0, ny=1 -> fx=0, fy=1 -> bottom-left.
        assert_eq!(corner(0, 4095, 180), (0, LCD_H - 1));
        // raw (max,max): nx=1, ny=1 -> fx=0, fy=0 -> top-left.
        assert_eq!(corner(4095, 4095, 180), (0, 0));
        // centre: both axes flipped -> 1-0.49988=0.50012 -> 240, 160.
        assert_eq!(corner(2047, 2047, 180), (240, 160));
    }

    #[test]
    fn rotation_270_swaps_and_flips_x() {
        // fx = 1 - nx, fy = ny.
        // raw (0,0): nx=0, ny=0 -> fx=1, fy=0 -> top-right.
        assert_eq!(corner(0, 0, 270), (LCD_W - 1, 0));
        // raw (max,0): nx=1, ny=0 -> fx=0, fy=0 -> top-left.
        assert_eq!(corner(4095, 0, 270), (0, 0));
        // raw (0,max): nx=0, ny=1 -> fx=1, fy=1 -> bottom-right.
        assert_eq!(corner(0, 4095, 270), (LCD_W - 1, LCD_H - 1));
        // raw (max,max): nx=1, ny=1 -> fx=0, fy=1 -> bottom-left.
        assert_eq!(corner(4095, 4095, 270), (0, LCD_H - 1));
        // centre: fx=1-nx=0.50012 -> 240; fy=ny=0.49988 -> 159.
        assert_eq!(corner(2047, 2047, 270), (240, 159));
    }

    #[test]
    fn out_of_range_rotation_falls_back_to_zero() {
        // Values that are not a multiple of 90 after rem_euclid(360) take the
        // `_` arm, which is the rotation-0 mapping. (360 -> 0; 1000 -> 280;
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
            for &xr in &[0, 1, 2047, 4094, 4095, 5000] {
                for &yr in &[0, 1, 2047, 4094, 4095, 5000] {
                    let (x, y) = map_to_lcd(xr, yr, X, Y, rot);
                    assert!((0..LCD_W).contains(&x), "x={x} rot={rot}");
                    assert!((0..LCD_H).contains(&y), "y={y} rot={rot}");
                }
            }
        }
    }

    #[test]
    fn non_full_ranges_normalize_before_placing() {
        // A driver that reports 200..3900 still maps its own min/max to the LCD
        // corners (the normalizer cancels the offset+span before the rotation).
        let xr = AxisRange {
            min: 200,
            max: 3900,
        };
        let yr = AxisRange {
            min: 150,
            max: 3950,
        };
        // Rotation-0 calibrated mapping: (min,min) -> top-right, (max,max) -> bottom-left.
        assert_eq!(map_to_lcd(200, 150, xr, yr, 0), (LCD_W - 1, 0));
        assert_eq!(map_to_lcd(3900, 3950, xr, yr, 0), (0, LCD_H - 1));
    }
}
