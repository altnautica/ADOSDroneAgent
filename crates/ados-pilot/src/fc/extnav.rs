//! When the FC's EKF began using external navigation, and since when its
//! local horizontal velocity has stayed still: an EKF that has just taken
//! up external navigation rings in velocity for seconds, and a vehicle that
//! lifts off meanwhile drifts by the error it carries.

/// The STATUSTEXT ArduPilot's EKF3 sends as it starts aiding from external
/// navigation (`EKF3 IMU%u is using external nav data`,
/// `AP_NavEKF3_Control.cpp`), at MAV_SEVERITY_INFO.
pub const USING_EXTNAV_TEXT: &str = "is using external nav data";
/// The horizontal LOCAL_POSITION_NED speed under which the estimate counts
/// as still, m/s (why: the pilot's `extnav_settle` readiness item).
pub const STILL_MPS: f32 = 0.01;

/// The external-navigation start and the stillness since.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ExtNavStart {
    /// When the EKF last said it started using external navigation.
    pub since_ms: Option<u64>,
    /// When the first LOCAL_POSITION_NED arrived: an EKF that took up
    /// external navigation before the pilot listened said so unheard.
    pub first_local_ms: Option<u64>,
    /// Since when every LOCAL_POSITION_NED has had a horizontal speed of at
    /// most [`STILL_MPS`].
    pub still_since_ms: Option<u64>,
}

impl ExtNavStart {
    /// A STATUSTEXT from the FC.
    pub fn on_text(&mut self, text: &str, now_ms: u64) {
        if text.contains(USING_EXTNAV_TEXT) {
            self.since_ms = Some(now_ms);
        }
    }

    /// A LOCAL_POSITION_NED from the FC.
    pub fn on_local(&mut self, vn: f32, ve: f32, now_ms: u64) {
        self.first_local_ms.get_or_insert(now_ms);
        if vn.hypot(ve) <= STILL_MPS {
            self.still_since_ms.get_or_insert(now_ms);
        } else {
            self.still_since_ms = None;
        }
    }

    /// How long the estimate has been still since external navigation began
    /// (since the first local position when that start was never heard), ms;
    /// `None` while it is not still.
    pub fn still_for_ms(&self, now_ms: u64) -> Option<u64> {
        let start = self.since_ms.or(self.first_local_ms)?;
        let still = self.still_since_ms?;
        Some(now_ms.saturating_sub(still.max(start)))
    }
}
