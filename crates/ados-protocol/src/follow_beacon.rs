//! `plugin.follow.beacon`: the operator's phone position, shared from a
//! ground station for a follow plugin to fuse with what the camera sees.
//!
//! A ground station that the operator lets share its position sends MAVLink
//! `FOLLOW_TARGET` (#144) toward the vehicle. The MAVLink router accepts it only
//! from an authorised ground link, drops stale, imprecise and too-frequent
//! fixes, and republishes each accepted fix on the plugin event bus as one
//! [`FollowBeacon`] on [`FOLLOW_BEACON_TOPIC`], through the plugin host's
//! event export socket (publisher `host`). A plugin reads the topic with
//! `event.subscribe` plus [`FOLLOW_BEACON_READ_CAP`]; no plugin may publish it.
//!
//! The sender fills `FOLLOW_TARGET` as follows:
//!
//! | field | meaning |
//! |---|---|
//! | `timestamp` | fix time, ms since the sender's boot |
//! | `lat`, `lon` | WGS84, degrees × 1e7 |
//! | `alt` | metres above mean sea level; NaN when unknown |
//! | `vel` | north, east, down, m/s; used only with [`EST_VEL`] (a phone measures ground velocity and sends down 0) |
//! | `position_cov` | `[horizontal accuracy m, vertical accuracy m, 0]` (the `eph epv` of the message definition); ≤ 0 or NaN is unknown |
//! | `est_capabilities` | [`EST_POS`] always, [`EST_VEL`] when `vel` is measured |

use serde::{Deserialize, Serialize};

/// The beacon payload version, carried as `v`.
pub const FOLLOW_BEACON_V1_VERSION: u32 = 1;

/// The bus topic the router publishes accepted fixes on.
pub const FOLLOW_BEACON_TOPIC: &str = "plugin.follow.beacon";

/// The capability a plugin needs, beside `event.subscribe`, to read the topic.
pub const FOLLOW_BEACON_READ_CAP: &str = "follow.beacon.read";

/// `source` of a fix that a ground station's phone sent as `FOLLOW_TARGET`.
pub const SOURCE_GCS_PHONE: &str = "gcs-phone";

/// MAVLink `FOLLOW_TARGET` message id.
pub const FOLLOW_TARGET_MSG_ID: u32 = 144;

/// `FOLLOW_TARGET.est_capabilities`: the position is valid.
pub const EST_POS: u8 = 1 << 0;
/// `FOLLOW_TARGET.est_capabilities`: the velocity is measured.
pub const EST_VEL: u8 = 1 << 1;
/// `FOLLOW_TARGET.est_capabilities`: the acceleration is measured.
pub const EST_ACCEL: u8 = 1 << 2;
/// `FOLLOW_TARGET.est_capabilities`: attitude and rates are measured.
pub const EST_ATT_RATES: u8 = 1 << 3;

/// A fix older than this, against the freshest fix seen from the same sender,
/// is dropped.
pub const MAX_FIX_AGE_MS: u64 = 2_000;

/// A fix whose horizontal accuracy is worse than this is dropped.
pub const MAX_HORIZ_ACC_M: f32 = 30.0;

/// Fewest milliseconds between two published fixes (10 Hz).
pub const MIN_INTERVAL_MS: u64 = 100;

/// One accepted fix, as published on [`FOLLOW_BEACON_TOPIC`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FollowBeacon {
    /// [`FOLLOW_BEACON_V1_VERSION`].
    pub v: u32,
    /// Latitude, degrees (WGS84).
    pub lat: f64,
    /// Longitude, degrees (WGS84).
    pub lon: f64,
    /// Altitude above mean sea level, metres; `None` when the sender had none.
    pub alt_m: Option<f64>,
    /// Velocity north, east, down, m/s; `None` when the sender did not measure it.
    pub vel_ned: Option<[f32; 3]>,
    /// Horizontal accuracy, metres (68 %).
    pub horiz_acc_m: f32,
    /// Vertical accuracy, metres; `None` when unknown.
    pub vert_acc_m: Option<f32>,
    /// ADOS time when the router received the fix, ns.
    pub stamp_ns: u64,
    /// Where the fix came from ([`SOURCE_GCS_PHONE`]).
    pub source: String,
    /// MAVLink system id of the sender.
    pub sysid: u8,
    /// MAVLink component id of the sender.
    pub compid: u8,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_beacon_serialises_to_the_documented_fields() {
        let b = FollowBeacon {
            v: FOLLOW_BEACON_V1_VERSION,
            lat: 47.397_742,
            lon: 8.545_594,
            alt_m: None,
            vel_ned: Some([1.0, -0.5, 0.0]),
            horiz_acc_m: 4.0,
            vert_acc_m: None,
            stamp_ns: 12_000_000_000,
            source: SOURCE_GCS_PHONE.to_string(),
            sysid: 255,
            compid: 190,
        };
        let v = serde_json::to_value(&b).unwrap();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "alt_m",
                "compid",
                "horiz_acc_m",
                "lat",
                "lon",
                "source",
                "stamp_ns",
                "sysid",
                "v",
                "vel_ned",
                "vert_acc_m"
            ]
        );
        assert!(v["alt_m"].is_null());
        assert_eq!(v["vel_ned"][1], -0.5);
        assert_eq!(serde_json::from_value::<FollowBeacon>(v).unwrap(), b);
    }
}
