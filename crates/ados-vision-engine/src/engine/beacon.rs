//! The follow beacon (`plugin.follow.beacon`, seam K) into the subject
//! estimate.
//!
//! The router republishes the operator's shared phone fix on the plugin event
//! bus through the export's publish op, which stamps it with the host's own
//! publisher id; a fix from any other publisher is ignored. A fix is placed
//! in the flight controller's local NED frame through its home
//! (`HOME_POSITION` carries home both on the globe and in that frame); with no
//! home reported yet, fixes are dropped. Each fix is queued and fused in time
//! order just before the first sighting that is not older than it, so a fix
//! arriving while a frame is still in inference never makes that frame stale
//! (see [`super::subject`]). Beacon-only tracking stays off.
//!
//! Clock: the estimate runs on the frame clock. Under simulation that is the
//! simulator's time, which the fix's `stamp_ns` (ADOS time at the router) is
//! on too; on a real node ADOS time is wall time, so the fix is stamped with
//! its arrival on the engine's monotonic clock, the one frames are captured
//! on.

use ados_protocol::follow_beacon::{FollowBeacon, FOLLOW_BEACON_TOPIC};
use ados_vision::subject::{ned_from_geodetic, BeaconFix, Geodetic};
use serde::Deserialize;

use super::VisionEngine;
use crate::depth::feed::Home;

/// The publisher id the plugin host stamps on what a host service publishes
/// through the export.
pub const BEACON_PUBLISHER: &str = "host";

/// The beacon fix in an export event line from the host; `None` for any
/// other line.
fn beacon_of(line: &[u8]) -> Option<FollowBeacon> {
    #[derive(Deserialize)]
    struct Event {
        topic: Option<String>,
        publisher: Option<String>,
        payload: Option<serde_json::Value>,
    }
    let ev = serde_json::from_slice::<Event>(line).ok()?;
    if ev.topic.as_deref() != Some(FOLLOW_BEACON_TOPIC)
        || ev.publisher.as_deref() != Some(BEACON_PUBLISHER)
    {
        return None;
    }
    serde_json::from_value(ev.payload?).ok()
}

/// The fix in the local NED frame of the flight controller whose home is
/// `home`. Without an altitude, the fix is taken at home's height.
fn local_ned(b: &FollowBeacon, home: &Home) -> [f32; 3] {
    let origin = Geodetic {
        lat_deg: home.lat_deg,
        lon_deg: home.lon_deg,
        alt_m: home.alt_m,
    };
    let fix = Geodetic {
        lat_deg: b.lat,
        lon_deg: b.lon,
        alt_m: b.alt_m.unwrap_or(home.alt_m),
    };
    let d = ned_from_geodetic(&origin, &fix);
    [0, 1, 2].map(|i| home.local_ned[i] + d[i] as f32)
}

impl VisionEngine {
    /// Apply one export event line arriving at `rx_ms` (engine clock): a
    /// beacon fix from the host is queued for the subject estimate. Returns
    /// whether it was.
    pub fn apply_beacon_line(&self, line: &[u8], rx_ms: u64) -> bool {
        let Some(b) = beacon_of(line) else {
            return false;
        };
        let Some(home) = self.range_feed.home() else {
            return false;
        };
        if !(b.lat.is_finite() && b.lon.is_finite() && b.horiz_acc_m.is_finite()) {
            return false;
        }
        let stamp_ns = if self.sim_pacing.get().is_some() {
            b.stamp_ns
        } else {
            rx_ms.saturating_mul(1_000_000)
        };
        self.subject_runtime().queue_beacon(BeaconFix {
            stamp_ns,
            ned: local_ned(&b, &home),
            horiz_acc_m: b.horiz_acc_m,
        });
        true
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use ados_protocol::follow_beacon::{FOLLOW_BEACON_V1_VERSION, ORIGIN_GROUND_LINK};

    pub(in crate::engine) fn home() -> Home {
        Home {
            lat_deg: 52.0,
            lon_deg: 4.5,
            alt_m: 10.0,
            local_ned: [1.0, -2.0, 0.5],
        }
    }

    pub(in crate::engine) fn line(publisher: &str, lat: f64, lon: f64) -> Vec<u8> {
        let b = FollowBeacon {
            v: FOLLOW_BEACON_V1_VERSION,
            lat,
            lon,
            alt_m: Some(8.0),
            vel_ned: None,
            horiz_acc_m: 3.0,
            vert_acc_m: None,
            stamp_ns: 5,
            origin: ORIGIN_GROUND_LINK.into(),
            link: "ws-1".into(),
            sysid: 255,
            compid: 190,
        };
        serde_json::to_vec(&serde_json::json!({
            "v": 1, "topic": FOLLOW_BEACON_TOPIC, "publisher": publisher,
            "ts_ms": 5, "payload": b,
        }))
        .unwrap()
    }

    #[test]
    fn a_host_fix_lands_in_the_local_frame_through_home() {
        let b = beacon_of(&line(BEACON_PUBLISHER, 52.0001, 4.5)).unwrap();
        let ned = local_ned(&b, &home());
        // 0.0001 degrees north of home is about 11.1 m; 2 m below home.
        assert!((ned[0] - 12.13).abs() < 0.05, "{ned:?}");
        assert!((ned[1] + 2.0).abs() < 1e-3 && (ned[2] - 2.5).abs() < 1e-3);
        assert!(beacon_of(&line("com.example.spoof", 52.0, 4.5)).is_none());
        assert!(beacon_of(b"{not json").is_none());
    }
}
