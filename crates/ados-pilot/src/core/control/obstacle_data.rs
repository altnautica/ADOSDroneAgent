//! What the pilot knows about its obstacle data: whether a lidar is expected,
//! and with avoid on, whether its scans are stale, missing altogether, or
//! arriving with nothing in them where something should be.

use ados_protocol::pilot::{AvoidMode, AvoidState, PlaceClass};

use super::super::Pilot;

impl Pilot {
    /// A lidar is expected on this vehicle: it scanned this boot or the
    /// capture service reports one. The one place that decides "never had"
    /// from "lost".
    pub(in crate::core) fn lidar_expected(&self) -> bool {
        self.lidar.expected()
    }

    /// A scan with no return taken at `ned`, tilted `tilt` (roll, pitch),
    /// cannot be open air: the place is confined, or the vehicle is armed
    /// and the surface it last saw this flight must still be in the scan
    /// ([`crate::sensor_health::LidarTrack::surface_in_reach`]). Such a
    /// scan is not obstacle data: it is never read as free space, and while
    /// only such scans arrive the data ages to `Stale`.
    pub(in crate::core) fn empty_scan_unbelievable(
        &mut self,
        ned: [f32; 3],
        tilt: [f32; 2],
    ) -> bool {
        self.settings.pilot.place_class == PlaceClass::Confined
            || (self.fc.armed && self.lidar.surface_in_reach(ned, tilt))
    }

    /// The obstacle data is stale only because the scans arriving carry no
    /// return where one should be ([`Self::empty_scan_unbelievable`]): the
    /// lidar is still scanning. The planar lidar never braked vertical
    /// motion, so a climb or descent out of the ambiguity stays flyable.
    pub(in crate::core) fn stale_on_empty_scans(&self, now_ms: u64) -> bool {
        self.obstacle_data_stale(now_ms)
            && self
                .lidar
                .scan_age_ms(now_ms)
                .is_some_and(|a| a <= crate::obstacles::SCAN_MAX_AGE_MS)
    }

    /// With avoid on, what is wrong with the obstacle data: `Stale` when a
    /// lidar is expected and its newest scan taken as obstacle data is older
    /// than the age limit, which includes a run of scans with no return
    /// where one should be ([`Self::empty_scan_unbelievable`]) (obstacles
    /// unknown, never clear: the pilot holds and flies no motion command);
    /// `Unavailable` when none is expected (nothing is braked for, sticks
    /// fly, Fly Here is refused). `None` with fresh scans or avoid off.
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
