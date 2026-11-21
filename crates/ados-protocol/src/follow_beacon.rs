//! `plugin.follow.beacon`: the operator's phone position, shared from a
//! ground station for a follow plugin to fuse with what the camera sees.
//!
//! A ground station that the operator lets share its position sends MAVLink
//! `FOLLOW_TARGET` (#144) toward the vehicle. The MAVLink router judges each
//! one, republishes an accepted fix on the plugin event bus as one
//! [`FollowBeacon`] on [`FOLLOW_BEACON_TOPIC`] through the plugin host's event
//! export socket (publisher `host`), and keeps the message off the flight
//! controller and every other endpoint. A plugin reads the topic with
//! `event.subscribe` plus [`FOLLOW_BEACON_READ_CAP`]; no plugin may publish it,
//! and no plugin may send `FOLLOW_TARGET` through the host.
//!
//! **Trust.** A beacon is low-trust: it says only that a ground link the
//! router admits for commands sent this position. [`FollowBeacon::origin`]
//! and [`FollowBeacon::link`] are stamped by the router; `sysid`/`compid` are
//! whatever the sender claimed. A consumer corroborates a beacon with what the
//! camera sees before acting on it, and anything that would follow on the
//! beacon alone holds to the one link the router has locked.
//!
//! The router accepts a fix only from an authorised ground link, a local
//! service or the relay (never an unauthenticated network peer, a plugin or the
//! plugin host's link); locks to one link until it has been silent for
//! [`LOCK_SILENCE_MS`]; drops fixes older than [`MAX_FIX_AGE_MS`] against the
//! link's least-delayed fix, repeated or reordered fixes, fixes with an
//! unknown or worse than [`MAX_HORIZ_ACC_M`] horizontal accuracy, and fixes
//! closer than [`MIN_INTERVAL_MS`] to the last one published.
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

/// [`FollowBeacon::origin`]: an authorised network ground link (the
/// WebSocket, TCP or UDP proxy, past the pairing posture check).
pub const ORIGIN_GROUND_LINK: &str = "ground_link";
/// [`FollowBeacon::origin`]: a local service on this node's MAVLink socket.
pub const ORIGIN_ON_BOX: &str = "on_box";
/// [`FollowBeacon::origin`]: a relay (the cloud relay, or a ground station
/// over the radio's aux uplink).
pub const ORIGIN_RELAY: &str = "relay";

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

/// A fix older than this, against the least-delayed fix of the same link, is
/// dropped.
pub const MAX_FIX_AGE_MS: u64 = 2_000;

/// A fix whose horizontal accuracy is worse than this is dropped.
pub const MAX_HORIZ_ACC_M: f32 = 30.0;

/// Fewest milliseconds between two published fixes (10 Hz).
pub const MIN_INTERVAL_MS: u64 = 100;

/// The router follows one link at a time; another link may take over once
/// the locked one has sent nothing for this long.
pub const LOCK_SILENCE_MS: u64 = 5_000;

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
    /// How the fix reached the router, as the router saw it:
    /// [`ORIGIN_GROUND_LINK`], [`ORIGIN_ON_BOX`] or [`ORIGIN_RELAY`].
    pub origin: String,
    /// The router's id for the connection the fix arrived on (`ws-3`, `tcp-1`,
    /// `udp-9f2c41d0`, `ipc-12`, `aux`), stable for the life of that connection.
    /// The router is locked to this link while it publishes from it.
    pub link: String,
    /// MAVLink system id the sender claimed.
    pub sysid: u8,
    /// MAVLink component id the sender claimed.
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
            origin: ORIGIN_GROUND_LINK.to_string(),
            link: "ws-3".to_string(),
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
                "link",
                "lon",
                "origin",
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
