//! On-device touch-calibration wizard state machine.
//!
//! The resistive overlay on the SPI LCD is rarely aligned with the visible
//! pixels, so a fresh panel falls back to the rotation-aware identity transform
//! (correct enough to land tab-bar taps, but visibly off). This module drives
//! the 9-point capture that fits a per-rig affine and persists it to
//! [`ados_hid::sidecar::TOUCH_CALIB_PATH`], after which the live UI maps taps
//! through the real calibration.
//!
//! The wizard is render-loop-owned, outside the navigator: while a controller is
//! active the loop paints the calibration screen and routes every tap here. The
//! controller is pure apart from the affine save on completion and the skip
//! marker, so the capture-and-fit progression is unit-tested with synthetic
//! taps.
//!
//! The operator can leave without a fit: a two-tap "Skip" button writes
//! [`TOUCH_CALIB_SKIPPED_PATH`] so the wizard does not re-open on every boot,
//! and a wizard that sees no tap for [`IDLE_TIMEOUT`] closes on its own (a
//! panel whose digitizer is absent or unreadable would otherwise hold the
//! status screen forever). A saved fit clears the skip marker.
//!
//! Targets and the RMS rejection threshold mirror the values the REST-side
//! session uses so the two capture paths never drift on geometry.

use std::path::Path;
use std::time::{Duration, Instant};

use ados_hid::affine::{self, SaveParams};

use crate::pages::TwoTapConfirm;
use crate::touch_input::{LCD_H, LCD_W};

/// The 9 calibration targets in LCD pixel coordinates — a 3x3 grid inset from
/// the panel edges. Nine points over-determine the six-unknown affine (18
/// equations), which drops the per-tap noise floor versus a 5-point fit.
pub const TARGETS: [(i32, i32); 9] = [
    (40, 40),
    (240, 40),
    (440, 40),
    (40, 160),
    (240, 160),
    (440, 160),
    (40, 280),
    (240, 280),
    (440, 280),
];

/// Reject a fit whose RMS residual exceeds this many LCD pixels and restart the
/// capture: a noisy or mis-tapped run should not persist a bad transform.
pub const REJECT_RMS_PX: f64 = 35.0;

/// One-shot recalibration request flag. A GCS "Recalibrate" action (or a manual
/// bench operator) writes this; the render loop consumes it to relaunch the
/// wizard on a panel that is already calibrated.
pub const RECALIBRATE_FLAG_PATH: &str = "/run/ados/recalibrate.flag";

/// Consume a pending recalibration request. Returns `true` when the flag was
/// present (and has been removed), so the render loop relaunches the wizard
/// exactly once per request. Missing flag returns `false`.
pub fn take_recalibrate_flag(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    // Best-effort unlink: even if the remove races another reader, returning
    // true at most relaunches an already-active wizard, which is a no-op.
    let _ = std::fs::remove_file(path);
    true
}

/// Persistent marker that the operator skipped calibration. While present the
/// render loop does not auto-launch the wizard at boot; an explicit
/// recalibration request still does.
pub const TOUCH_CALIB_SKIPPED_PATH: &str = "/etc/ados/touch.calib.skipped";

/// The wizard closes itself after this long without a tap.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// The Skip button in panel coordinates `(x, y, w, h)` for an instruction
/// block at `block_y`. It sits right of centre under the instruction text, in
/// the gap between two target columns, so a tap aimed at a reticle does not
/// land on it.
pub fn skip_button_rect(block_y: i32) -> (i32, i32, i32, i32) {
    (300, block_y + 72, 100, 32)
}

/// The instruction block's top edge for a target at height `target_y`: the
/// half of the panel away from the target, so text never sits under it.
pub fn instruction_block_y(target_y: i32) -> i32 {
    let panel_h = crate::pages::PANEL_H as i32;
    if target_y < panel_h / 2 {
        (panel_h * 2) / 3
    } else {
        panel_h / 4
    }
}

/// The Skip button's confirm key.
const SKIP_KEY: &str = "calibration.skip";

/// What a tap did to the wizard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalibrationOutcome {
    /// More targets remain (or the fit failed and the capture restarted, or a
    /// first Skip tap armed the button).
    Continue,
    /// All targets captured, the fit passed, and the calibration was saved.
    Saved,
    /// The operator confirmed Skip; the skip marker was written.
    Skipped,
}

/// The 9-point capture state machine. Holds the raw ADC sample collected for
/// each target so far; on the final tap it fits the affine and persists it.
pub struct CalibrationController {
    rotation: i32,
    samples: Vec<(i32, i32)>,
    /// True when the previous run was rejected (over-RMS or singular) and the
    /// capture restarted, so the screen can tell the operator to try again.
    failed_last: bool,
    /// RMS residual of the last fit attempt, for logging / display.
    last_rms: Option<f64>,
    /// When the wizard last saw a tap (or started).
    last_activity: Instant,
    /// Two-tap arm state of the Skip button.
    skip_confirm: TwoTapConfirm,
}

impl CalibrationController {
    /// Start a fresh capture for the configured display `rotation` (recorded in
    /// the saved blob so a later rotation change can invalidate the fit).
    pub fn new(rotation: i32) -> Self {
        Self {
            rotation,
            samples: Vec::new(),
            failed_last: false,
            last_rms: None,
            last_activity: Instant::now(),
            skip_confirm: TwoTapConfirm::default(),
        }
    }

    /// Whether the wizard has gone [`IDLE_TIMEOUT`] without a tap at `now`.
    pub fn idle_expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_activity) >= IDLE_TIMEOUT
    }

    /// Whether a first Skip tap is waiting for its confirming tap.
    pub fn skip_armed(&self) -> bool {
        self.skip_confirm.is_armed(SKIP_KEY)
    }

    /// Route one tap. `pos` is the tap in panel coordinates through the
    /// current (uncalibrated) transform, used only to hit the Skip button;
    /// `raw` is the ADC sample a target capture records.
    ///
    /// A tap on Skip arms it, and a confirming second tap writes the skip
    /// marker and returns [`CalibrationOutcome::Skipped`]; a Skip tap is never
    /// recorded as a sample. Any other tap is a sample (see
    /// [`Self::on_tap_raw`]), and a saved fit removes the skip marker.
    pub fn on_tap(
        &mut self,
        pos: (i32, i32),
        raw: (i32, i32),
        calib_path: &Path,
        skip_marker: &Path,
    ) -> CalibrationOutcome {
        self.last_activity = Instant::now();
        let (bx, by, bw, bh) = skip_button_rect(instruction_block_y(self.current_target().1));
        let on_skip = (bx..bx + bw).contains(&pos.0) && (by..by + bh).contains(&pos.1);
        if on_skip {
            if !self.skip_confirm.tap(SKIP_KEY) {
                return CalibrationOutcome::Continue;
            }
            if let Err(e) = std::fs::write(skip_marker, b"skipped\n") {
                tracing::warn!(error = %e, "touch calibration skip marker write failed");
            }
            return CalibrationOutcome::Skipped;
        }
        self.skip_confirm.disarm();
        let outcome = self.on_tap_raw(raw, calib_path);
        if outcome == CalibrationOutcome::Saved {
            let _ = std::fs::remove_file(skip_marker);
        }
        outcome
    }

    /// Total number of targets in the capture.
    pub fn target_count(&self) -> usize {
        TARGETS.len()
    }

    /// Index of the target awaiting a tap (clamped so a momentary full sample
    /// set never indexes out of range).
    pub fn current_index(&self) -> usize {
        self.samples.len().min(TARGETS.len() - 1)
    }

    /// The pixel coordinate of the target awaiting a tap.
    pub fn current_target(&self) -> (i32, i32) {
        TARGETS[self.current_index()]
    }

    /// Whether the last fit was rejected and the capture restarted.
    pub fn failed(&self) -> bool {
        self.failed_last
    }

    /// RMS residual of the last fit attempt in LCD pixels, if one ran.
    pub fn last_rms(&self) -> Option<f64> {
        self.last_rms
    }

    /// Record a raw ADC tap for the current target.
    ///
    /// Returns [`CalibrationOutcome::Continue`] while targets remain. On the
    /// final tap it fits the affine: a clean fit (RMS within
    /// [`REJECT_RMS_PX`]) is saved to `calib_path` and returns
    /// [`CalibrationOutcome::Saved`]; an over-RMS, singular, or unwritable fit
    /// restarts the capture (`failed()` becomes true) and returns `Continue`:
    /// a bad fit is never persisted.
    pub fn on_tap_raw(&mut self, raw: (i32, i32), calib_path: &Path) -> CalibrationOutcome {
        self.failed_last = false;
        self.samples.push(raw);
        if self.samples.len() < TARGETS.len() {
            return CalibrationOutcome::Continue;
        }

        match affine::compute_from_samples(&self.samples, &TARGETS) {
            Ok((aff, rms)) if rms <= REJECT_RMS_PX => {
                self.last_rms = Some(rms);
                let params = SaveParams {
                    rotation: self.rotation,
                    rms,
                    lcd_size: (LCD_W, LCD_H),
                    ..SaveParams::default()
                };
                match affine::save(&aff, calib_path, &params) {
                    Ok(()) => CalibrationOutcome::Saved,
                    Err(e) => {
                        tracing::warn!(error = %e, "touch calibration save failed; restarting capture");
                        self.restart();
                        CalibrationOutcome::Continue
                    }
                }
            }
            Ok((_, rms)) => {
                tracing::info!(
                    rms,
                    threshold = REJECT_RMS_PX,
                    "touch calibration residual over threshold; restarting capture"
                );
                self.last_rms = Some(rms);
                self.restart();
                CalibrationOutcome::Continue
            }
            Err(e) => {
                tracing::info!(error = %e, "touch calibration fit failed; restarting capture");
                self.last_rms = None;
                self.restart();
                CalibrationOutcome::Continue
            }
        }
    }

    /// Clear the collected samples and flag the restart so the screen prompts a
    /// retry.
    fn restart(&mut self) {
        self.samples.clear();
        self.failed_last = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthesize a raw ADC point that maps to `target` under a clean affine
    /// (raw = target * 8 + offset), so a full sweep fits with near-zero RMS.
    fn raw_for(target: (i32, i32)) -> (i32, i32) {
        (target.0 * 8 + 100, target.1 * 8 + 50)
    }

    #[test]
    fn nine_clean_taps_save_a_calibration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("touch.calib");
        let mut ctrl = CalibrationController::new(0);

        // The first eight taps advance without saving.
        for (i, target) in TARGETS.iter().take(8).enumerate() {
            assert_eq!(ctrl.current_index(), i);
            assert_eq!(
                ctrl.on_tap_raw(raw_for(*target), &path),
                CalibrationOutcome::Continue
            );
            assert!(!path.exists(), "must not persist before the final tap");
        }
        // The ninth tap fits and saves.
        assert_eq!(
            ctrl.on_tap_raw(raw_for(TARGETS[8]), &path),
            CalibrationOutcome::Saved
        );
        // A valid calibration matrix is now on disk.
        assert!(affine::load(&path).is_some());
        assert!(ctrl.last_rms().unwrap() <= REJECT_RMS_PX);
        assert!(!ctrl.failed());
    }

    #[test]
    fn degenerate_taps_restart_without_saving() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("touch.calib");
        let mut ctrl = CalibrationController::new(0);

        // All nine taps land on the same raw point -> the fit is singular, the
        // capture restarts (no skip), and nothing is written.
        for _ in 0..8 {
            assert_eq!(
                ctrl.on_tap_raw((500, 500), &path),
                CalibrationOutcome::Continue
            );
        }
        assert_eq!(
            ctrl.on_tap_raw((500, 500), &path),
            CalibrationOutcome::Continue
        );
        assert!(ctrl.failed(), "a rejected fit flags the retry");
        assert_eq!(ctrl.current_index(), 0, "capture restarted at target 0");
        assert!(!path.exists(), "a rejected fit never persists");
    }

    #[test]
    fn recalibrate_flag_is_consumed_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recalibrate.flag");
        assert!(!take_recalibrate_flag(&path));
        std::fs::write(&path, "1\n").unwrap();
        assert!(take_recalibrate_flag(&path), "first read sees the flag");
        assert!(!path.exists(), "the flag is removed after consumption");
        assert!(!take_recalibrate_flag(&path), "a second read sees nothing");
    }

    /// Centre of the Skip button for the controller's current target.
    fn skip_point(ctrl: &CalibrationController) -> (i32, i32) {
        let (x, y, w, h) = skip_button_rect(instruction_block_y(ctrl.current_target().1));
        (x + w / 2, y + h / 2)
    }

    /// One Skip tap only arms it; the confirming tap writes the marker and
    /// exits. Neither tap is recorded as a calibration sample.
    #[test]
    fn skip_takes_two_taps_and_writes_the_marker() {
        let dir = tempfile::tempdir().unwrap();
        let calib = dir.path().join("touch.calib");
        let marker = dir.path().join("touch.calib.skipped");
        let mut ctrl = CalibrationController::new(0);
        let at = skip_point(&ctrl);
        assert_eq!(
            ctrl.on_tap(at, (1, 1), &calib, &marker),
            CalibrationOutcome::Continue
        );
        assert!(ctrl.skip_armed());
        assert!(!marker.exists(), "an armed skip writes nothing yet");
        assert_eq!(ctrl.current_index(), 0, "a skip tap is not a sample");
        assert_eq!(
            ctrl.on_tap(at, (1, 1), &calib, &marker),
            CalibrationOutcome::Skipped
        );
        assert!(marker.exists());
        assert!(!calib.exists());
    }

    /// A target tap between the two Skip taps disarms it, so a stray Skip tap
    /// never combines with a later one.
    #[test]
    fn a_sample_tap_disarms_skip() {
        let dir = tempfile::tempdir().unwrap();
        let calib = dir.path().join("touch.calib");
        let marker = dir.path().join("touch.calib.skipped");
        let mut ctrl = CalibrationController::new(0);
        let at = skip_point(&ctrl);
        ctrl.on_tap(at, (1, 1), &calib, &marker);
        let target = TARGETS[0];
        assert_eq!(
            ctrl.on_tap(target, raw_for(target), &calib, &marker),
            CalibrationOutcome::Continue
        );
        assert!(!ctrl.skip_armed());
        assert_eq!(ctrl.current_index(), 1);
        let at = skip_point(&ctrl);
        assert_eq!(
            ctrl.on_tap(at, (1, 1), &calib, &marker),
            CalibrationOutcome::Continue
        );
        assert!(!marker.exists());
    }

    /// A completed fit removes an earlier skip marker so the panel counts as
    /// calibrated again.
    #[test]
    fn a_saved_fit_clears_the_skip_marker() {
        let dir = tempfile::tempdir().unwrap();
        let calib = dir.path().join("touch.calib");
        let marker = dir.path().join("touch.calib.skipped");
        std::fs::write(&marker, "skipped\n").unwrap();
        let mut ctrl = CalibrationController::new(0);
        let mut last = CalibrationOutcome::Continue;
        for target in TARGETS {
            last = ctrl.on_tap(target, raw_for(target), &calib, &marker);
        }
        assert_eq!(last, CalibrationOutcome::Saved);
        assert!(!marker.exists());
    }

    /// The wizard expires after the idle window, and a tap restarts the clock.
    #[test]
    fn idle_timeout_counts_from_the_last_tap() {
        let dir = tempfile::tempdir().unwrap();
        let calib = dir.path().join("touch.calib");
        let marker = dir.path().join("touch.calib.skipped");
        let mut ctrl = CalibrationController::new(0);
        let start = Instant::now();
        assert!(!ctrl.idle_expired(start + IDLE_TIMEOUT - Duration::from_secs(1)));
        assert!(ctrl.idle_expired(start + IDLE_TIMEOUT + Duration::from_secs(1)));
        std::thread::sleep(Duration::from_millis(20));
        let target = TARGETS[0];
        ctrl.on_tap(target, raw_for(target), &calib, &marker);
        assert!(!ctrl.idle_expired(start + IDLE_TIMEOUT + Duration::from_millis(10)));
    }
}
