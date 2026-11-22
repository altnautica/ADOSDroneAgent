//! The FC's EKF_STATUS_REPORT as the `position` readiness item reads it.

use std::collections::HashMap;

/// An EKF_STATUS_REPORT older than this no longer vouches for the estimate.
pub const EKF_STATUS_STALE_MS: u64 = 3000;
/// Horizontal position test ratio that blocks: ArduPilot's default EKF
/// failsafe threshold (`FS_EKF_THRESH` 0.8).
pub const EKF_POS_VARIANCE_BLOCK: f32 = 0.8;

/// A relative position source is enabled: ArduPilot's `optflow.enabled() ||
/// visual_odom.enabled()` (`FLOW_TYPE` or `VISO_TYPE` non-zero).
pub fn relative_enabled(params: &HashMap<String, f32>) -> bool {
    let on = |n: &str| params.get(n).is_some_and(|p| *p != 0.0);
    on("FLOW_TYPE") || on("VISO_TYPE")
}

/// Since when the EKF's horizontal position has held good without a break,
/// per kind. A report that loses it, or a gap of [`EKF_STATUS_STALE_MS`]
/// between reports, restarts the clock.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct EkfSettle {
    /// An absolute horizontal position (what a GPS source set needs).
    pub abs_since_ms: Option<u64>,
    /// Any position ArduPilot's own check accepts: absolute, or relative
    /// with a relative source enabled.
    pub any_since_ms: Option<u64>,
}

impl EkfSettle {
    /// Fold one report in; `gap` is true when the previous report was
    /// missing or stale.
    pub fn step(&mut self, v: &EkfPosition, armed: bool, relative: bool, gap: bool, now_ms: u64) {
        let usable = v.horiz_variance < EKF_POS_VARIANCE_BLOCK;
        let keep = |since: Option<u64>, good: bool| match (good, since) {
            (false, _) => None,
            (true, Some(s)) if !gap => Some(s),
            (true, _) => Some(now_ms),
        };
        self.abs_since_ms = keep(self.abs_since_ms, usable && v.has_position(armed, false));
        self.any_since_ms = keep(self.any_since_ms, usable && v.has_position(armed, relative));
    }
}

/// The FC's EKF_STATUS_REPORT, as far as the position item reads it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct EkfPosition {
    /// `EKF_POS_HORIZ_ABS`: the EKF has an absolute horizontal position.
    pub horiz_abs: bool,
    /// `EKF_PRED_POS_HORIZ_ABS`: a predicted absolute horizontal position.
    pub pred_horiz_abs: bool,
    /// `EKF_POS_HORIZ_REL`: a horizontal position relative to the start.
    pub horiz_rel: bool,
    /// `EKF_PRED_POS_HORIZ_REL`: a predicted relative horizontal position.
    pub pred_horiz_rel: bool,
    /// `EKF_CONST_POS_MODE`: the EKF holds a constant position (no aiding).
    pub const_pos_mode: bool,
    /// The horizontal position innovation test ratio (1.0 rejects).
    pub horiz_variance: f32,
}

impl EkfPosition {
    /// ArduCopter's own position test (`ArduCopter/system.cpp`, Copter-4.6.2
    /// lines 239-294; `ekf_check.cpp:53` takes relative or absolute):
    /// disarmed it accepts a predicted absolute or, with a relative source
    /// enabled, a predicted relative position; armed it needs an absolute or
    /// enabled relative position outside constant-position mode.
    /// `relative_enabled` stands for `optflow.enabled() ||
    /// visual_odom.enabled()` (`FLOW_TYPE` or `VISO_TYPE` non-zero).
    pub fn has_position(&self, armed: bool, relative_enabled: bool) -> bool {
        let (abs, rel) = if armed {
            (
                self.horiz_abs && !self.const_pos_mode,
                self.horiz_rel && !self.const_pos_mode,
            )
        } else {
            (self.horiz_abs || self.pred_horiz_abs, self.pred_horiz_rel)
        };
        abs || (relative_enabled && rel)
    }
}
