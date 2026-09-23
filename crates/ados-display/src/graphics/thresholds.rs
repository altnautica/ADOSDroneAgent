//! The grading table: one pair of cut points per metric, shared by every page.
//!
//! A metric shown on two surfaces of the same frame must read the same color on
//! both, so no page carries its own cut points. Each [`Grade`] is applied through
//! [`Palette::threshold_color`] with [`Palette::grade`].

use embedded_graphics::pixelcolor::Rgb888;

use crate::graphics::palette::{Palette, ThresholdDirection};

/// Two cut points and the direction that is good.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Grade {
    /// At or past this (in the good direction) the value reads success.
    pub success_at: f64,
    /// At or past this the value reads warning; beyond it, error.
    pub warning_at: f64,
    pub direction: ThresholdDirection,
}

/// Radio link RSSI in dBm.
pub const RSSI_DBM: Grade = Grade {
    success_at: -65.0,
    warning_at: -80.0,
    direction: ThresholdDirection::HigherIsBetter,
};

/// Radio packet loss in percent.
pub const LOSS_PCT: Grade = Grade {
    success_at: 1.0,
    warning_at: 5.0,
    direction: ThresholdDirection::LowerIsBetter,
};

/// Decoded video frame rate.
pub const VIDEO_FPS: Grade = Grade {
    success_at: 25.0,
    warning_at: 15.0,
    direction: ThresholdDirection::HigherIsBetter,
};

/// Glass-to-glass video latency in milliseconds.
pub const VIDEO_LATENCY_MS: Grade = Grade {
    success_at: 80.0,
    warning_at: 150.0,
    direction: ThresholdDirection::LowerIsBetter,
};

/// Host CPU utilisation in percent.
pub const CPU_PCT: Grade = Grade {
    success_at: 70.0,
    warning_at: 85.0,
    direction: ThresholdDirection::LowerIsBetter,
};

/// Host RAM in use, in percent of total.
pub const RAM_PCT: Grade = Grade {
    success_at: 70.0,
    warning_at: 85.0,
    direction: ThresholdDirection::LowerIsBetter,
};

/// Host root filesystem in use, in percent of total.
pub const DISK_PCT: Grade = Grade {
    success_at: 80.0,
    warning_at: 90.0,
    direction: ThresholdDirection::LowerIsBetter,
};

/// SoC temperature in degrees Celsius.
pub const TEMP_C: Grade = Grade {
    success_at: 65.0,
    warning_at: 75.0,
    direction: ThresholdDirection::LowerIsBetter,
};

/// Flight battery remaining in percent.
pub const BATTERY_PCT: Grade = Grade {
    success_at: 50.0,
    warning_at: 20.0,
    direction: ThresholdDirection::HigherIsBetter,
};

impl Palette {
    /// Color `value` by `grade`. An absent value reads the muted tertiary tone.
    pub fn grade(&self, value: Option<f64>, grade: Grade) -> Rgb888 {
        self.threshold_color(value, grade.success_at, grade.warning_at, grade.direction)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::palette::DARK;

    #[test]
    fn a_grade_reads_success_warning_error_in_its_direction() {
        assert_eq!(DARK.grade(Some(-60.0), RSSI_DBM), DARK.status_success);
        assert_eq!(DARK.grade(Some(-72.0), RSSI_DBM), DARK.status_warning);
        assert_eq!(DARK.grade(Some(-88.0), RSSI_DBM), DARK.status_error);
        assert_eq!(DARK.grade(Some(75.0), RAM_PCT), DARK.status_warning);
        assert_eq!(DARK.grade(Some(0.5), LOSS_PCT), DARK.status_success);
        assert_eq!(DARK.grade(Some(9.0), LOSS_PCT), DARK.status_error);
        assert_eq!(DARK.grade(None, VIDEO_FPS), DARK.text_tertiary);
    }
}
