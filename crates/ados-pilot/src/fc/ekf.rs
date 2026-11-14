//! The FC's EKF_STATUS_REPORT as the `position` readiness item reads it.

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
