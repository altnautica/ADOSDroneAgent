//! The Linux evdev side of the touch reader: node discovery, range adoption
//! and the async event drain.

use super::{AxisRange, TouchEvent, TouchTransformHandle, DEFAULT_TOUCH_NODE};

/// Discover, open, and drain the touchscreen evdev node for the life of the
/// process, sending each completed [`TouchEvent`] on `tx`. A missing node or a
/// read error never ends the reader: [`crate::touch_supervisor::supervise_touch`]
/// marks touch absent, waits the fixed local-socket pace and rediscovers, so a
/// late-loading driver or a replugged panel comes back without a restart. It
/// returns only once the render loop dropped its receiver.
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
pub async fn run_touch_reader(
    handle: TouchTransformHandle,
    tx: tokio::sync::mpsc::Sender<TouchEvent>,
) {
    crate::touch_supervisor::supervise_touch(
        handle,
        tx,
        discover_touch_device,
        |(path, device), handle, tx| drain_touch_device(path, device, handle, tx),
    )
    .await
}

/// One open device's lifetime: adopt its ranges, then classify strokes until
/// the device errors or the render loop is gone.
async fn drain_touch_device(
    path: String,
    device: evdev::Device,
    handle: TouchTransformHandle,
    tx: tokio::sync::mpsc::Sender<TouchEvent>,
) -> crate::touch_supervisor::SessionEnd {
    use std::time::Instant;

    use ados_hmi::touch::StrokeFsm;
    use evdev::{AbsoluteAxisType, InputEventKind, Key};

    use crate::touch_supervisor::SessionEnd;

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

    let mut stream = match device.into_event_stream() {
        Ok(s) => s,
        Err(e) => return SessionEnd::Lost(e),
    };
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
            Err(e) => return SessionEnd::Lost(e),
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
                            return SessionEnd::ChannelClosed;
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
