// Unused in this crate today: the touch/LCD path is owned by the display layer.
// Kept as the host-portable gesture FSM for that layer to wire when its touch
// path lands; do not delete here.
//
//! Touch input: raw evdev samples -> rotated/calibrated -> classified gesture.
//!
//! Ports `src/ados/services/ui/touch/bridge.py`: the per-stroke state machine
//! (`open_stroke` / `record_move` / `close_stroke`), the move filter, and the
//! tap / long_press / swipe / drag classifier. The chunk-1 [`crate::affine`]
//! transform maps each raw `(x, y)` ADC sample to LCD pixels before the FSM
//! sees it.
//!
//! The FSM + classifier ([`StrokeFsm`]) are PURE and host-portable: they take
//! transformed `(x_lcd, y_lcd, ts_ms)` samples, so the move-delta filter, the
//! displacement/duration thresholds, and the direction-of-larger-component rule
//! are all unit-testable with synthetic strokes. Only the evdev node-open +
//! read loop is target-gated to Linux.

use crate::affine::Affine;

/// Move filter: only record a sample when it differs from the previous accepted
/// one by at least this many pixels on either axis (stops a held-stationary pen
/// from flooding the move stream).
pub const MOVE_DELTA_PX: i32 = 2;

/// A contact under this duration AND under [`TAP_DISPLACEMENT_PX`] is a tap;
/// at/over this duration it is a long press.
pub const TAP_DUR_MS: i64 = 400;

/// Displacement floor (LCD px) separating a tap/long_press from a swipe/drag.
pub const TAP_DISPLACEMENT_PX: f64 = 12.0;

/// A contact under this duration with displacement >= [`SWIPE_DISPLACEMENT_PX`]
/// is a swipe; otherwise (with motion) it is a drag.
pub const SWIPE_DUR_MS: i64 = 250;

/// Displacement floor (LCD px) for a fast contact to count as a swipe.
pub const SWIPE_DISPLACEMENT_PX: f64 = 24.0;

/// The classified kind of a completed stroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GestureKind {
    Tap,
    LongPress,
    Swipe,
    Drag,
}

impl GestureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            GestureKind::Tap => "tap",
            GestureKind::LongPress => "long_press",
            GestureKind::Swipe => "swipe",
            GestureKind::Drag => "drag",
        }
    }
}

/// Cardinal direction of the larger displacement component. Set for swipes and
/// drags; `None` for tap/long_press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Up => "up",
            Direction::Down => "down",
            Direction::Left => "left",
            Direction::Right => "right",
        }
    }
}

/// A single accepted move sample in LCD pixel space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TouchMove {
    pub x_lcd: i32,
    pub y_lcd: i32,
    pub timestamp_ms: i64,
}

/// A completed pen-down -> pen-up sequence, classified into a kind.
#[derive(Debug, Clone, PartialEq)]
pub struct TouchGesture {
    pub kind: GestureKind,
    pub start_x: i32,
    pub start_y: i32,
    pub end_x: i32,
    pub end_y: i32,
    pub start_t_ms: i64,
    pub end_t_ms: i64,
    pub duration_ms: i64,
    pub direction: Option<Direction>,
    pub velocity_px_per_s: f64,
    pub samples: Vec<TouchMove>,
}

/// One step the FSM emits as it consumes a transformed sample. The bridge maps
/// `Move` onto its live move stream and `Gesture` onto the gesture stream.
#[derive(Debug, Clone, PartialEq)]
pub enum StrokeStep {
    /// An accepted move sample passed the delta filter.
    Move(TouchMove),
    /// A pen-up closed the stroke into a classified gesture.
    Gesture(TouchGesture),
}

/// Pure per-stroke state machine + gesture classifier. The bridge feeds it
/// pen-down / transformed-move / pen-up events; it tracks the stroke and emits
/// move + gesture steps. No evdev, no IO, fully unit-testable.
#[derive(Debug, Default)]
pub struct StrokeFsm {
    down_at_ms: i64,
    down_x_lcd: i32,
    down_y_lcd: i32,
    last_x_lcd: i32,
    last_y_lcd: i32,
    samples: Vec<TouchMove>,
    pen_down: bool,
}

impl StrokeFsm {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the pen is currently down (a stroke is open).
    pub fn pen_down(&self) -> bool {
        self.pen_down
    }

    /// Begin a stroke at `now_ms`. Coordinate seeding is deferred to the first
    /// accepted move (only then is there a sample), matching the Python
    /// `_open_stroke`.
    pub fn open_stroke(&mut self, now_ms: i64) {
        self.down_at_ms = now_ms;
        self.samples.clear();
        self.down_x_lcd = 0;
        self.down_y_lcd = 0;
        self.last_x_lcd = 0;
        self.last_y_lcd = 0;
        self.pen_down = true;
    }

    /// Record a transformed `(x_lcd, y_lcd)` move. The first sample seeds the
    /// down-position and is always accepted; later samples pass only when they
    /// differ from the previous accepted one by >= [`MOVE_DELTA_PX`] on either
    /// axis. Returns the accepted sample, or `None` when filtered.
    pub fn record_move(&mut self, x_lcd: i32, y_lcd: i32, now_ms: i64) -> Option<TouchMove> {
        if self.samples.is_empty() {
            self.down_x_lcd = x_lcd;
            self.down_y_lcd = y_lcd;
            self.last_x_lcd = x_lcd;
            self.last_y_lcd = y_lcd;
            let m = TouchMove {
                x_lcd,
                y_lcd,
                timestamp_ms: now_ms,
            };
            self.samples.push(m);
            return Some(m);
        }
        let dx = (x_lcd - self.last_x_lcd).abs();
        let dy = (y_lcd - self.last_y_lcd).abs();
        if dx < MOVE_DELTA_PX && dy < MOVE_DELTA_PX {
            return None;
        }
        self.last_x_lcd = x_lcd;
        self.last_y_lcd = y_lcd;
        let m = TouchMove {
            x_lcd,
            y_lcd,
            timestamp_ms: now_ms,
        };
        self.samples.push(m);
        Some(m)
    }

    /// Close the stroke at `now_ms` and classify it. Returns `None` when no
    /// sample landed before pen-up (a driver quirk, nothing to emit). Resets
    /// the FSM to the pen-up state.
    pub fn close_stroke(&mut self, now_ms: i64) -> Option<TouchGesture> {
        self.pen_down = false;
        let last = self.samples.last().copied()?;
        let end_x_lcd = last.x_lcd;
        let end_y_lcd = last.y_lcd;
        let duration_ms = (now_ms - self.down_at_ms).max(0);
        let dx_total = end_x_lcd - self.down_x_lcd;
        let dy_total = end_y_lcd - self.down_y_lcd;
        let displacement = ((dx_total * dx_total + dy_total * dy_total) as f64).sqrt();
        let velocity = if duration_ms > 0 {
            displacement * 1000.0 / duration_ms as f64
        } else {
            0.0
        };
        let kind = classify(duration_ms, displacement);
        let direction = match kind {
            GestureKind::Swipe | GestureKind::Drag => Some(direction_for(dx_total, dy_total)),
            _ => None,
        };
        let gesture = TouchGesture {
            kind,
            start_x: self.down_x_lcd,
            start_y: self.down_y_lcd,
            end_x: end_x_lcd,
            end_y: end_y_lcd,
            start_t_ms: self.down_at_ms,
            end_t_ms: now_ms,
            duration_ms,
            direction,
            velocity_px_per_s: velocity,
            samples: std::mem::take(&mut self.samples),
        };
        Some(gesture)
    }
}

/// Classify a stroke by duration + total displacement. Mirrors the Python
/// `_classify`: under the displacement floor it is a tap/long_press by duration;
/// a fast long-enough move is a swipe; otherwise a drag.
pub fn classify(duration_ms: i64, displacement: f64) -> GestureKind {
    if displacement < TAP_DISPLACEMENT_PX {
        return if duration_ms >= TAP_DUR_MS {
            GestureKind::LongPress
        } else {
            GestureKind::Tap
        };
    }
    if duration_ms < SWIPE_DUR_MS && displacement >= SWIPE_DISPLACEMENT_PX {
        return GestureKind::Swipe;
    }
    GestureKind::Drag
}

/// Cardinal direction of the larger displacement component (ties favour the
/// horizontal axis, matching `abs(dx) >= abs(dy)`).
pub fn direction_for(dx: i32, dy: i32) -> Direction {
    if dx.abs() >= dy.abs() {
        if dx >= 0 {
            Direction::Right
        } else {
            Direction::Left
        }
    } else if dy >= 0 {
        Direction::Down
    } else {
        Direction::Up
    }
}

/// The initial transform for a stroke source: a persisted calibration if one
/// exists, else the rotation-aware identity. Mirrors the Python
/// `_initial_affine`.
pub fn initial_affine(calib_path: &std::path::Path, rotation: i32, lcd_size: (i32, i32)) -> Affine {
    crate::affine::load(calib_path)
        .unwrap_or_else(|| crate::affine::identity_for(rotation, lcd_size))
}

/// Run the touch evdev read loop on Linux: discover the node, then drain raw
/// ABS_X/ABS_Y + BTN_TOUCH + SYN events, applying `affine` to each sample and
/// feeding the [`StrokeFsm`]. Invokes `on_step` for each emitted move/gesture.
/// The node-open + read is the only hardware-coupled part; the FSM stays pure.
#[cfg(target_os = "linux")]
pub fn run_event_loop<F>(
    device_path: &str,
    affine: &Affine,
    fsm: &mut StrokeFsm,
    mut on_step: F,
) -> std::io::Result<()>
where
    F: FnMut(StrokeStep),
{
    use std::time::Instant;

    use evdev::{Device, InputEventKind, Key};

    let mut dev = Device::open(device_path)?;
    let base = Instant::now();

    // Raw ABS coordinates accumulate between SYN_REPORT markers.
    let mut pending_x: Option<i32> = None;
    let mut pending_y: Option<i32> = None;

    loop {
        for ev in dev.fetch_events()? {
            let now_ms = base.elapsed().as_millis() as i64;
            match ev.kind() {
                InputEventKind::Key(Key::BTN_TOUCH) => {
                    if ev.value() == 1 && !fsm.pen_down() {
                        fsm.open_stroke(now_ms);
                    } else if ev.value() == 0 && fsm.pen_down() {
                        if let Some(g) = fsm.close_stroke(now_ms) {
                            on_step(StrokeStep::Gesture(g));
                        }
                        pending_x = None;
                        pending_y = None;
                    }
                }
                InputEventKind::AbsAxis(axis) => {
                    if axis == evdev::AbsoluteAxisType::ABS_X {
                        pending_x = Some(ev.value());
                    } else if axis == evdev::AbsoluteAxisType::ABS_Y {
                        pending_y = Some(ev.value());
                    }
                }
                InputEventKind::Synchronization(_) if fsm.pen_down() => {
                    if let (Some(x), Some(y)) = (pending_x, pending_y) {
                        let (x_lcd, y_lcd) = affine.apply(x, y);
                        if let Some(m) = fsm.record_move(x_lcd, y_lcd, now_ms) {
                            on_step(StrokeStep::Move(m));
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a stroke through the FSM with `(x_lcd, y_lcd, ts_ms)` samples
    /// (already transformed) and return the closing gesture plus the count of
    /// accepted move samples.
    fn run_stroke(
        open_ms: i64,
        samples: &[(i32, i32, i64)],
        close_ms: i64,
    ) -> (TouchGesture, usize) {
        let mut fsm = StrokeFsm::new();
        fsm.open_stroke(open_ms);
        let mut moves = 0;
        for &(x, y, t) in samples {
            if fsm.record_move(x, y, t).is_some() {
                moves += 1;
            }
        }
        let g = fsm.close_stroke(close_ms).expect("a gesture");
        (g, moves)
    }

    #[test]
    fn tap_is_short_and_still() {
        // Single point, released 100 ms later -> tap, no direction.
        let (g, moves) = run_stroke(0, &[(100, 100, 0)], 100);
        assert_eq!(g.kind, GestureKind::Tap);
        assert!(g.direction.is_none());
        assert_eq!(moves, 1);
        assert_eq!((g.start_x, g.start_y), (100, 100));
        assert_eq!((g.end_x, g.end_y), (100, 100));
    }

    #[test]
    fn long_press_is_long_and_still() {
        // Held 400 ms at one point (displacement 0) -> long_press.
        let (g, _) = run_stroke(0, &[(100, 100, 0)], 400);
        assert_eq!(g.kind, GestureKind::LongPress);
        assert!(g.direction.is_none());
    }

    #[test]
    fn just_under_long_threshold_is_tap() {
        let (g, _) = run_stroke(0, &[(50, 50, 0)], 399);
        assert_eq!(g.kind, GestureKind::Tap);
    }

    #[test]
    fn small_displacement_under_floor_stays_tap() {
        // 8 px total displacement (< 12) -> still tap even though it moved.
        let (g, _) = run_stroke(0, &[(0, 0, 0), (8, 0, 50)], 100);
        assert_eq!(g.kind, GestureKind::Tap);
    }

    #[test]
    fn fast_long_move_is_a_swipe_right() {
        // 200 ms (< 250) with 40 px displacement (>= 24) -> swipe, +x -> right.
        let (g, _) = run_stroke(0, &[(0, 0, 0), (40, 0, 100)], 200);
        assert_eq!(g.kind, GestureKind::Swipe);
        assert_eq!(g.direction, Some(Direction::Right));
        assert!(g.velocity_px_per_s > 0.0);
    }

    #[test]
    fn swipe_up_when_vertical_component_larger_and_negative() {
        // dy = -50 dominates dx = 10 -> up.
        let (g, _) = run_stroke(0, &[(0, 0, 0), (10, -50, 100)], 200);
        assert_eq!(g.kind, GestureKind::Swipe);
        assert_eq!(g.direction, Some(Direction::Up));
    }

    #[test]
    fn slow_long_move_is_a_drag() {
        // 300 ms (>= swipe dur) with 40 px displacement -> drag, not swipe.
        let (g, _) = run_stroke(0, &[(0, 0, 0), (40, 0, 150)], 300);
        assert_eq!(g.kind, GestureKind::Drag);
        assert_eq!(g.direction, Some(Direction::Right));
    }

    #[test]
    fn fast_short_move_above_tap_floor_below_swipe_floor_is_a_drag() {
        // 100 ms, displacement 16 px: above tap floor (12) but below swipe
        // floor (24) -> drag.
        let (g, _) = run_stroke(0, &[(0, 0, 0), (16, 0, 50)], 100);
        assert_eq!(g.kind, GestureKind::Drag);
    }

    #[test]
    fn move_delta_filter_drops_sub_threshold_samples() {
        let mut fsm = StrokeFsm::new();
        fsm.open_stroke(0);
        // First sample seeds (accepted).
        assert!(fsm.record_move(100, 100, 0).is_some());
        // +1 px on each axis is below the 2 px floor -> dropped.
        assert!(fsm.record_move(101, 101, 10).is_none());
        // +2 px clears the floor -> accepted.
        assert!(fsm.record_move(102, 100, 20).is_some());
    }

    #[test]
    fn close_without_any_sample_emits_nothing() {
        let mut fsm = StrokeFsm::new();
        fsm.open_stroke(0);
        // Pen-up before any ABS sample landed -> no gesture.
        assert!(fsm.close_stroke(100).is_none());
        assert!(!fsm.pen_down());
    }

    #[test]
    fn samples_are_carried_on_the_gesture_and_fsm_resets() {
        let mut fsm = StrokeFsm::new();
        fsm.open_stroke(0);
        fsm.record_move(0, 0, 0);
        fsm.record_move(40, 0, 100);
        let g = fsm.close_stroke(200).unwrap();
        assert_eq!(g.samples.len(), 2);
        // A fresh stroke starts clean.
        fsm.open_stroke(1000);
        fsm.record_move(5, 5, 1000);
        let g2 = fsm.close_stroke(1100).unwrap();
        assert_eq!(g2.samples.len(), 1);
    }

    #[test]
    fn direction_ties_favour_horizontal() {
        // |dx| == |dy| -> horizontal wins (matches abs(dx) >= abs(dy)).
        assert_eq!(direction_for(10, 10), Direction::Right);
        assert_eq!(direction_for(-10, 10), Direction::Left);
        assert_eq!(direction_for(0, 5), Direction::Down);
        assert_eq!(direction_for(0, -5), Direction::Up);
    }

    #[test]
    fn classify_thresholds_at_the_boundaries() {
        // Exactly the tap displacement floor (12) is NOT a tap (>= floor path).
        assert_eq!(classify(100, 12.0), GestureKind::Drag);
        // Just under the floor is a tap.
        assert_eq!(classify(100, 11.9), GestureKind::Tap);
        // At the long-press duration with no displacement.
        assert_eq!(classify(400, 0.0), GestureKind::LongPress);
        // Swipe boundary: dur 249 (< 250), disp 24 (>= 24).
        assert_eq!(classify(249, 24.0), GestureKind::Swipe);
        // dur 250 (not < 250) -> drag even at swipe displacement.
        assert_eq!(classify(250, 24.0), GestureKind::Drag);
    }

    #[test]
    fn affine_transform_feeds_the_fsm() {
        // A 2x scale identity-ish matrix: raw 50 -> 100 px confirms the bridge
        // applies the transform before the FSM classifies.
        let m = Affine {
            a: 2.0,
            b: 0.0,
            c: 0.0,
            d: 0.0,
            e: 2.0,
            f: 0.0,
        };
        let mut fsm = StrokeFsm::new();
        fsm.open_stroke(0);
        let (x0, y0) = m.apply(0, 0);
        let (x1, y1) = m.apply(20, 0); // -> 40 px, a swipe-distance move
        fsm.record_move(x0, y0, 0);
        fsm.record_move(x1, y1, 100);
        let g = fsm.close_stroke(200).unwrap();
        assert_eq!(g.end_x, 40);
        assert_eq!(g.kind, GestureKind::Swipe);
    }

    #[test]
    fn gesture_and_direction_wire_strings() {
        assert_eq!(GestureKind::Tap.as_str(), "tap");
        assert_eq!(GestureKind::LongPress.as_str(), "long_press");
        assert_eq!(GestureKind::Swipe.as_str(), "swipe");
        assert_eq!(GestureKind::Drag.as_str(), "drag");
        assert_eq!(Direction::Up.as_str(), "up");
        assert_eq!(Direction::Down.as_str(), "down");
        assert_eq!(Direction::Left.as_str(), "left");
        assert_eq!(Direction::Right.as_str(), "right");
    }
}
