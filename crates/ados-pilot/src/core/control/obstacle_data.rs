//! What the pilot knows about its obstacle data: whether a lidar is expected,
//! and with avoid on, whether its scans are stale or missing altogether.

use ados_protocol::pilot::{AvoidMode, AvoidState};

use super::super::Pilot;

impl Pilot {
    /// A lidar is expected on this vehicle: it scanned this boot or the
    /// capture service reports one. The one place that decides "never had"
    /// from "lost".
    pub(in crate::core) fn lidar_expected(&self) -> bool {
        self.lidar.expected()
    }

    /// With avoid on, what is wrong with the obstacle data: `Stale` when a
    /// lidar is expected and its newest scan is older than the age limit
    /// (obstacles unknown, never clear: the pilot holds and flies no motion
    /// command); `Unavailable` when none is expected (nothing is braked for,
    /// sticks fly, Fly Here is refused). `None` with fresh scans or avoid off.
    pub(in crate::core) fn obstacle_data_state(&self, now_ms: u64) -> Option<AvoidState> {
        if self.settings.pilot.avoid == AvoidMode::Off || self.obstacles.scan_fresh(now_ms) {
            None
        } else if self.lidar_expected() {
            Some(AvoidState::Stale)
        } else {
            Some(AvoidState::Unavailable)
        }
    }

    pub(in crate::core) fn obstacle_data_stale(&self, now_ms: u64) -> bool {
        self.obstacle_data_state(now_ms) == Some(AvoidState::Stale)
    }

    /// Avoid is on but there is nothing to avoid with: brakes never act.
    pub(super) fn obstacle_data_unavailable(&self, now_ms: u64) -> bool {
        self.obstacle_data_state(now_ms) == Some(AvoidState::Unavailable)
    }
}
