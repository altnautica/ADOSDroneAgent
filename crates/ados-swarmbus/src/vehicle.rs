//! Filling this node's own beacon from the flight controller's fused state.
//!
//! The MAVLink router owns vehicle state and publishes a snapshot on
//! `/run/ados/state.sock` at ~10 Hz; this reads that snapshot rather than opening a
//! second path to the flight controller. [`beacon_from_state`] is the whole
//! conversion, kept pure so the field mapping is testable without a socket.
//!
//! Every field the beacon carries already exists in the snapshot in the encoding
//! the beacon wants, because both are MAVLink `GLOBAL_POSITION_INT` derivatives —
//! so this is a scale, never a re-derivation.

use serde_json::Value;

use crate::beacon::{
    SwarmBeacon, STATUS_ARMED, STATUS_EMERGENCY, STATUS_GPS_OK, STATUS_GUIDED, STATUS_HERO,
};
use crate::ModePrecedence;

/// The lowest `GPS_RAW_INT.fix_type` that counts as a usable fix
/// (`GPS_FIX_TYPE_3D_FIX`).
///
/// A 2D fix has no altitude, and the separation layer works in three dimensions:
/// a neighbour whose altitude is a guess must not be separated against as though
/// it were measured. Below this the [`STATUS_GPS_OK`] bit stays clear and the
/// position is broadcast anyway, flagged — every consumer can then decide, rather
/// than the position simply vanishing from the fleet view.
pub const MIN_USABLE_FIX_TYPE: i64 = 3;

/// Read a nested `f64` out of the state snapshot, e.g. `("position", "lat")`.
fn nested_f64(state: &Value, group: &str, field: &str) -> Option<f64> {
    state.get(group)?.get(field)?.as_f64()
}

/// Saturating conversion of a scaled float to the beacon's integer field.
///
/// Saturating rather than wrapping: a garbage or infinite reading must clamp to
/// the representable extreme, never wrap a northbound velocity into a southbound
/// one. `as` on a float in Rust already saturates and maps NaN to 0, which is the
/// behaviour wanted; this names it so the intent is not mistaken for an accident.
fn saturating_i32(v: f64) -> i32 {
    v as i32
}

/// Same for the 16-bit fields.
fn saturating_i16(v: f64) -> i16 {
    v as i16
}

/// Build this node's beacon from a state snapshot.
///
/// `slot` and `seq_ms` come from the caller (the config and the service's own
/// uptime); everything else comes from the snapshot. An absent or non-object
/// snapshot yields a beacon carrying only the slot and sequence, with a zeroed
/// position and every condition bit clear — which reads, correctly, as "this node
/// is on the bus but has no fix and is not armed".
///
/// `hero` is read from the snapshot's `video_profile` extra, which the video
/// service publishes as `"hero"` or `"thumbnail"`. An absent key reads as
/// thumbnail, matching the boot default: a fleet powering up together must not
/// have every drone claiming the full-video allocation.
///
/// Three fields are agent-maintained rather than vehicle-sourced, and all three
/// arrive as snapshot extras because their producers run in other processes:
/// `video_profile` from the video service (above), and `swarm_precedence` +
/// `swarm_emergency` from the onboard autonomy layer in the MAVLink router, which
/// owns the FC link its control loop commands through. Each has a safe absent
/// reading — thumbnail, `hold`, not-in-override — so a node running none of those
/// layers radiates an honest beacon rather than a defaulted-to-plausible one.
pub fn beacon_from_state(state: Option<&Value>, slot: u8, seq_ms: u16) -> SwarmBeacon {
    let mut b = SwarmBeacon {
        slot,
        seq_ms,
        ..SwarmBeacon::default()
    };
    let Some(state) = state else {
        return b;
    };

    b.lat = nested_f64(state, "position", "lat")
        .map(|v| saturating_i32(v * 1e7))
        .unwrap_or(0);
    b.lon = nested_f64(state, "position", "lon")
        .map(|v| saturating_i32(v * 1e7))
        .unwrap_or(0);
    // Home-relative altitude, in decimetres. `alt_msl` is deliberately not used:
    // every separation threshold is relative, and mixing the two references across
    // a fleet would put two drones at "the same altitude" tens of metres apart.
    b.alt_dm = nested_f64(state, "position", "alt_rel")
        .map(|v| saturating_i16(v * 10.0))
        .unwrap_or(0);
    b.vx_cms = nested_f64(state, "velocity", "vx")
        .map(|v| saturating_i16(v * 100.0))
        .unwrap_or(0);
    b.vy_cms = nested_f64(state, "velocity", "vy")
        .map(|v| saturating_i16(v * 100.0))
        .unwrap_or(0);
    b.vz_cms = nested_f64(state, "velocity", "vz")
        .map(|v| saturating_i16(v * 100.0))
        .unwrap_or(0);

    let mut status = 0u8;
    if state.get("armed").and_then(Value::as_bool).unwrap_or(false) {
        status |= STATUS_ARMED;
    }
    // The bus advertises whether this vehicle is in a mode that accepts the
    // offboard setpoints the autonomy layer sends. Which mode that is depends on
    // the firmware -- ArduPilot's GUIDED and PX4's OFFBOARD mean the same thing
    // here -- so matching only the name "guided" made every PX4 neighbour render
    // as un-commandable to the whole fleet regardless of what it was actually
    // doing. Upper-cased first because the wire value is the flight
    // controller's own mode string and the shared predicate matches the decoded
    // spelling.
    if state
        .get("mode")
        .and_then(Value::as_str)
        .is_some_and(|m| ados_protocol::accepts_offboard_setpoints(&m.to_ascii_uppercase()))
    {
        status |= STATUS_GUIDED;
    }
    if state
        .get("gps")
        .and_then(|g| g.get("fix_type"))
        .and_then(Value::as_i64)
        .unwrap_or(0)
        >= MIN_USABLE_FIX_TYPE
    {
        status |= STATUS_GPS_OK;
    }
    if state
        .get("video_profile")
        .and_then(Value::as_str)
        .is_some_and(|p| p == "hero")
    {
        status |= STATUS_HERO;
    }
    // The emergency bit and the precedence field come from the onboard autonomy
    // layer, which runs in the MAVLink router (it owns the FC link the control loop
    // commands through). It publishes both into this snapshot as `swarm_precedence`
    // and `swarm_emergency`, so the two processes meet in exactly one place — the
    // same seam `video_profile` uses for the hero bit.
    //
    // An absent key reads as `hold` / not-in-override, which is the pre-Phase-5
    // steady state: a node with no autonomy layer running radiates zeroes in the
    // precedence field and no emergency bit, and every reader honestly decodes it as
    // "this drone is not being flown by the swarm layer".
    if state
        .get("swarm_emergency")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        status |= STATUS_EMERGENCY;
    }
    b.status = status;
    b.set_precedence(
        state
            .get("swarm_precedence")
            .and_then(Value::as_str)
            .map(ModePrecedence::from_wire)
            .unwrap_or_default(),
    );
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::beacon::STATUS_PRECEDENCE_MASK;
    use crate::ModePrecedence;
    use serde_json::json;

    fn flying() -> Value {
        json!({
            "armed": true,
            "mode": "GUIDED",
            "position": {"lat": 12.9716, "lon": -77.5946, "alt_msl": 920.0, "alt_rel": 32.5},
            "velocity": {"vx": 1.2, "vy": -0.4, "vz": -0.07},
            "gps": {"fix_type": 3, "satellites": 14},
        })
    }

    #[test]
    fn a_flying_snapshot_maps_every_field_into_the_beacon() {
        let b = beacon_from_state(Some(&flying()), 3, 41234);
        assert_eq!(b.slot, 3);
        assert_eq!(b.seq_ms, 41234);
        assert_eq!(b.lat, 129_716_000);
        assert_eq!(b.lon, -775_946_000);
        assert_eq!(b.alt_dm, 325, "alt_rel in decimetres");
        assert_eq!(b.vx_cms, 120);
        assert_eq!(b.vy_cms, -40);
        assert_eq!(b.vz_cms, -7);
        assert!(b.armed() && b.guided() && b.gps_ok());
        assert!(!b.hero() && !b.emergency());
        // The precedence field has no source here and stays zero, so every reader
        // decodes `hold` until the autonomy layer sets it.
        assert_eq!(b.status & STATUS_PRECEDENCE_MASK, 0);
        assert_eq!(b.precedence(), ModePrecedence::Hold);
    }

    /// The autonomy layer publishes its ACTIVE level into the snapshot and the beacon
    /// must carry it, because that is the only way a peer or the operator learns which
    /// level actually governs this aircraft rather than which was commanded. Mode
    /// ambiguity of exactly this kind is implicated in a long series of
    /// supervisory-control losses.
    #[test]
    fn the_precedence_field_tracks_the_swarm_precedence_extra() {
        for level in ModePrecedence::ARBITRATION_ORDER {
            let mut s = flying();
            s["swarm_precedence"] = json!(level.as_wire());
            let b = beacon_from_state(Some(&s), 3, 0);
            assert_eq!(b.precedence(), level, "{level:?} did not reach the beacon");
            // It survives the wire, so a peer decodes the same level.
            assert_eq!(
                SwarmBeacon::decode(&b.encode()).unwrap().precedence(),
                level
            );
            // And the condition bits are untouched by the precedence write.
            assert!(b.armed() && b.guided() && b.gps_ok());
        }
    }

    /// An unknown or malformed level must read as `hold`, never as an authority the
    /// reader would act on, and never as a panic: a newer router in a mixed-version
    /// fleet is a realistic mid-upgrade state.
    #[test]
    fn an_unknown_precedence_string_degrades_to_hold() {
        for bad in [
            json!("orbit"),
            json!(""),
            json!(null),
            json!(4),
            json!(true),
        ] {
            let mut s = flying();
            s["swarm_precedence"] = bad.clone();
            let b = beacon_from_state(Some(&s), 3, 0);
            assert_eq!(b.precedence(), ModePrecedence::Hold, "{bad}");
            assert_eq!(b.status & STATUS_PRECEDENCE_MASK, 0, "{bad} left bits set");
        }
        // And the round trip through the string table is exact for every real level.
        for level in ModePrecedence::ARBITRATION_ORDER {
            assert_eq!(ModePrecedence::from_wire(level.as_wire()), level);
        }
    }

    /// The emergency bit says the separation layer has taken over. It must be the
    /// router's flag verbatim, and it must not be inferrable from anything else —
    /// a drone whose separation layer is holding it away from a neighbour is a
    /// different situation from one merely flying a formation.
    #[test]
    fn the_emergency_bit_tracks_the_swarm_emergency_extra() {
        let mut engaged = flying();
        engaged["swarm_emergency"] = json!(true);
        engaged["swarm_precedence"] = json!("hard-separation");
        let b = beacon_from_state(Some(&engaged), 3, 0);
        assert!(b.emergency());
        assert_eq!(b.precedence(), ModePrecedence::HardSeparation);
        // Still armed/guided/fixed: the override is an addition, not a replacement.
        assert!(b.armed() && b.guided() && b.gps_ok());

        // Absent, false, or a non-bool all read as not-in-override.
        for quiet in [
            None,
            Some(json!(false)),
            Some(json!(null)),
            Some(json!("yes")),
        ] {
            let mut s = flying();
            if let Some(v) = quiet.clone() {
                s["swarm_emergency"] = v;
            }
            let quiet_beacon = beacon_from_state(Some(&s), 3, 0);
            assert!(
                quiet_beacon.status & STATUS_EMERGENCY == 0,
                "{quiet:?} must not raise the emergency bit"
            );
        }
    }

    /// `alt_rel`, not `alt_msl`. Mixing the two references across a fleet would put
    /// two drones at "the same altitude" hundreds of metres apart — this snapshot
    /// has an `alt_msl` of 920 m specifically to catch that.
    #[test]
    fn altitude_is_home_relative_never_mean_sea_level() {
        let b = beacon_from_state(Some(&flying()), 1, 0);
        assert_eq!(b.alt_dm, 325);
        assert!((b.alt_m() - 32.5).abs() < 1e-9);
    }

    /// An empty or absent snapshot must produce a beacon that reads as "on the bus,
    /// no fix, not armed" rather than one that looks like a vehicle at 0°N 0°E with
    /// a good fix.
    #[test]
    fn an_absent_or_empty_snapshot_yields_a_flagless_beacon() {
        for state in [None, Some(&json!({})), Some(&json!("not an object"))] {
            let b = beacon_from_state(state, 9, 7);
            assert_eq!(b.slot, 9);
            assert_eq!(b.seq_ms, 7);
            assert_eq!(b.status, 0, "no condition bit may be set without a source");
            assert!(!b.gps_ok(), "no fix is claimed");
            assert_eq!((b.lat, b.lon, b.alt_dm), (0, 0, 0));
            assert_eq!((b.vx_cms, b.vy_cms, b.vz_cms), (0, 0, 0));
        }
    }

    /// A partial snapshot must not poison the fields it does carry. This is the
    /// steady state on a drone whose GPS has not locked yet.
    #[test]
    fn a_partial_snapshot_carries_what_it_has_and_zeroes_the_rest() {
        let b = beacon_from_state(
            Some(&json!({"armed": true, "position": {"lat": 1.0}})),
            2,
            0,
        );
        assert!(b.armed());
        assert_eq!(b.lat, 10_000_000);
        assert_eq!(b.lon, 0, "an absent longitude is zero, not garbage");
        assert!(!b.gps_ok());
        assert!(!b.guided());
    }

    /// The GPS gate is a threshold, not an equality: fix types above 3D (RTK) are
    /// better fixes and must not read as unusable.
    #[test]
    fn the_gps_bit_is_a_threshold_at_a_three_dimensional_fix() {
        let with_fix =
            |t: i64| beacon_from_state(Some(&json!({"gps": {"fix_type": t}})), 1, 0).gps_ok();
        assert!(!with_fix(0), "no fix");
        assert!(!with_fix(1), "no fix");
        assert!(!with_fix(2), "2D has no altitude");
        assert!(with_fix(3), "3D");
        assert!(with_fix(4), "DGPS");
        assert!(with_fix(5), "RTK float");
        assert!(with_fix(6), "RTK fixed");
        assert_eq!(MIN_USABLE_FIX_TYPE, 3);
    }

    /// The guided bit gates whether the autonomy layer may command this vehicle, so
    /// the mode match must not be fooled by case or by a different mode.
    #[test]
    fn the_guided_bit_matches_the_mode_string_case_insensitively() {
        let in_mode = |m: &str| beacon_from_state(Some(&json!({"mode": m})), 1, 0).guided();
        assert!(in_mode("GUIDED"));
        assert!(in_mode("guided"));
        assert!(in_mode("Guided"));
        // PX4 has no mode called GUIDED. Its equivalent is OFFBOARD, so a PX4
        // vehicle that was correctly placed in the one mode that accepts our
        // setpoints used to advertise itself to the whole fleet as
        // un-commandable, and nothing an operator could do would change that.
        assert!(in_mode("OFFBOARD"));
        assert!(in_mode("offboard"));
        assert!(in_mode("GUIDED_NOGPS"));
        assert!(!in_mode("STABILIZE"));
        assert!(!in_mode("AUTO"));
        // PX4 ignores offboard setpoints under AUTO, so admitting these would
        // advertise a vehicle as commandable while its commands went nowhere.
        assert!(!in_mode("AUTO.LOITER"));
        assert!(!in_mode("AUTO.MISSION"));
        assert!(!in_mode("LOITER"));
        assert!(!in_mode(""));
        // A non-string mode does not panic and does not claim guided.
        assert!(!beacon_from_state(Some(&json!({"mode": 4})), 1, 0).guided());
    }

    /// The hero bit is the video service's flag, and an absent key must read as
    /// thumbnail — the boot default that stops 24 drones each claiming the full
    /// video allocation.
    #[test]
    fn the_hero_bit_tracks_the_video_profile_extra_and_defaults_to_thumbnail() {
        let profile = |p: Value| {
            let mut s = flying();
            s["video_profile"] = p;
            beacon_from_state(Some(&s), 1, 0).hero()
        };
        assert!(profile(json!("hero")));
        assert!(!profile(json!("thumbnail")));
        assert!(!profile(json!("HERO")), "the wire value is exactly `hero`");
        assert!(!profile(json!(null)));
        assert!(!profile(json!(true)));
        assert!(
            !beacon_from_state(Some(&flying()), 1, 0).hero(),
            "absent key"
        );
    }

    /// An out-of-range reading must clamp, never wrap: a wrapped velocity turns a
    /// northbound neighbour into a southbound one, which is exactly the input that
    /// would make the separation layer steer into it.
    #[test]
    fn out_of_range_readings_saturate_rather_than_wrapping() {
        let b = beacon_from_state(
            Some(&json!({
                "position": {"lat": 1e9, "lon": -1e9, "alt_rel": 1e9},
                "velocity": {"vx": 1e9, "vy": -1e9, "vz": -1e9},
            })),
            1,
            0,
        );
        assert_eq!(b.lat, i32::MAX);
        assert_eq!(b.lon, i32::MIN);
        assert_eq!(b.alt_dm, i16::MAX);
        assert_eq!(b.vx_cms, i16::MAX);
        assert_eq!(b.vy_cms, i16::MIN);
        assert_eq!(b.vz_cms, i16::MIN);
    }

    /// JSON has no infinity and no NaN, so a non-finite flight-controller reading
    /// cannot reach this function as a number at all — `serde_json` renders it as
    /// `null`, which reads as "no reading" and yields 0.
    ///
    /// Worth pinning rather than assuming: 0 m/s is the honest answer for a velocity
    /// the vehicle could not report, and it is emphatically NOT the saturated
    /// extreme a reader might expect from the clamping above. A `null` position
    /// likewise reads as 0, and the accompanying GPS bit stays clear, so no consumer
    /// mistakes it for a fix at the equator.
    #[test]
    fn a_non_finite_reading_arrives_as_null_and_reads_as_no_reading() {
        let wire = json!({
            "position": {"lat": f64::NAN, "lon": f64::INFINITY},
            "velocity": {"vx": f64::INFINITY, "vy": f64::NEG_INFINITY, "vz": f64::NAN},
        });
        // The premise: serde_json cannot represent these, so they are already null.
        assert_eq!(wire["velocity"]["vx"], json!(null));
        assert_eq!(wire["position"]["lat"], json!(null));

        let b = beacon_from_state(Some(&wire), 1, 0);
        assert_eq!((b.vx_cms, b.vy_cms, b.vz_cms), (0, 0, 0));
        assert_eq!((b.lat, b.lon), (0, 0));
        assert!(!b.gps_ok(), "a zeroed position must not read as a fix");
    }

    /// The whole point of the pure conversion: a filled beacon survives the wire
    /// unchanged, so what a neighbour decodes is what the flight controller said.
    #[test]
    fn a_filled_beacon_round_trips_through_the_wire() {
        let b = beacon_from_state(Some(&flying()), 3, 41234);
        assert_eq!(SwarmBeacon::decode(&b.encode()), Some(b));
    }
}
