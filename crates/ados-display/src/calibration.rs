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
//! controller is pure (the only side effect is the affine save on completion),
//! so the capture-and-fit progression is unit-tested with synthetic taps.
//!
//! Targets and the RMS rejection threshold mirror the values the REST-side
//! session uses so the two capture paths never drift on geometry.

use std::path::Path;

use ados_hid::affine::{self, SaveParams};

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

/// What a tap did to the wizard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalibrationOutcome {
    /// More targets remain (or the fit failed and the capture restarted).
    Continue,
    /// All targets captured, the fit passed, and the calibration was saved.
    Saved,
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
        }
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
    /// restarts the capture (`failed()` becomes true) and returns `Continue`,
    /// so the wizard never exits without a good calibration.
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
}
