//! The seam between the transport and the control laws.
//!
//! `ados-swarmbus` owns the radio, the beacon wire format and the neighbour
//! table; this crate owns the control laws. Everything that crosses between them
//! crosses here, in one place, so the laws stay pure functions over
//! [`crate::NeighborState`] and can be tested with no socket and no clock.
//!
//! # Which direction each half travels
//!
//! The control loop runs inside `ados-mavlink-router`, because the autonomy layer commands
//! the flight controller through `FcConnection::send_msg` and the router owns that
//! link. So the two halves are different processes and the seam is two IPC
//! payloads, not a function call:
//!
//! * INBOUND — the neighbour table, read off `/run/ados/swarm.sock`. That payload
//!   is a published contract (`ados_swarmbus::publish::neighbors_payload`), which
//!   is why [`fixes_from_payload`] pins every key name it reads in its tests: a
//!   rename upstream must fail here loudly rather than quietly leave the autonomy
//!   layer flying on an empty neighbour table.
//! * OUTBOUND — the active precedence and the emergency condition, published in
//!   the router's own state snapshot as [`EXTRA_PRECEDENCE`] and
//!   [`EXTRA_EMERGENCY`], which `ados_swarmbus::vehicle::beacon_from_state` folds
//!   into the beacon exactly as it already does for the hero bit.
//!
//! # Staleness
//!
//! The payload reports each neighbour's `age_ms` and the transport prunes past
//! `NEIGHBOR_STALE` before publishing, so an aircraft that has gone silent simply
//! is not in the array. [`fixes_from_payload`] re-checks the age anyway: the
//! router is a separate process and a socket that stopped delivering would
//! otherwise leave it replaying the last snapshot forever, which is precisely the
//! "never fly on stale data" failure the age check exists to prevent.

use ados_swarmbus::NEIGHBOR_STALE;
use serde_json::Value;

use crate::controller::NeighborFix;

/// State-snapshot key carrying this node's active precedence level, as
/// `ModePrecedence::as_wire`. Absent reads as `hold`.
pub const EXTRA_PRECEDENCE: &str = "swarm_precedence";

/// State-snapshot key carrying this node's emergency condition (beacon status bit
/// 2). Absent reads as false.
pub const EXTRA_EMERGENCY: &str = "swarm_emergency";

/// Parse the `/run/ados/swarm.sock` payload into the geodetic fixes the controller
/// consumes, dead-reckoned forward to now.
///
/// `since_received` is how long ago this payload arrived. It is added to each
/// entry's published `age_ms`, and it is what turns a 2 Hz feed into a 10 Hz one:
/// the socket delivers five times slower than the control loop runs, so without
/// it every fifth tick would carry a genuine prediction and the other four would
/// replay the same frozen frame — a staircase, at exactly the scale that matters
/// to a 4 m separation floor.
///
/// Writes into `out` rather than returning a `Vec`: this runs at 10 Hz on an SBC
/// that is also encoding video.
///
/// An entry missing any field it needs is SKIPPED rather than defaulted, and so
/// is an entry whose position is PRESENT but is not a measurement — see
/// [`is_measured_fix`]. A neighbour with no position is not a neighbour at the
/// origin: that would put a phantom aircraft at the equator and drag the whole
/// flock toward it.
pub fn fixes_from_payload(
    payload: &Value,
    since_received: std::time::Duration,
    out: &mut Vec<NeighborFix>,
) {
    out.clear();
    let Some(neighbors) = payload.get("neighbors").and_then(Value::as_array) else {
        return;
    };
    let own_sender = sender_id(payload.get("sender_id"));
    let extra_ms = since_received.as_millis() as u64;
    for n in neighbors {
        let Some(slot) = n.get("slot").and_then(Value::as_u64) else {
            continue;
        };
        let (Some(lat_deg), Some(lon_deg), Some(alt_m)) = (
            n.get("lat").and_then(Value::as_f64),
            n.get("lon").and_then(Value::as_f64),
            n.get("alt_m").and_then(Value::as_f64),
        ) else {
            continue;
        };
        let status = status_byte(n);
        if !is_measured_fix(lat_deg, lon_deg, alt_m, status) {
            continue;
        }
        let age_ms = n
            .get("age_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .saturating_add(extra_ms);
        if age_ms >= NEIGHBOR_STALE.as_millis() as u64 {
            continue;
        }
        let vn = n.get("vx_ms").and_then(Value::as_f64).unwrap_or(0.0);
        let ve = n.get("vy_ms").and_then(Value::as_f64).unwrap_or(0.0);
        let vd = n.get("vz_ms").and_then(Value::as_f64).unwrap_or(0.0);
        // Dead reckon at constant velocity.
        let dt = age_ms as f64 / 1000.0;
        let origin = crate::geo::GeoOrigin::new(lat_deg, lon_deg, alt_m);
        let (lat_deg, lon_deg, alt_m) =
            origin.to_geo(crate::geo::Ned::new(vn * dt, ve * dt, vd * dt));
        out.push(NeighborFix {
            slot: slot.min(u8::MAX as u64) as u8,
            lat_deg,
            lon_deg,
            alt_m,
            vn,
            ve,
            vd,
            status,
            sender_order: match (own_sender, sender_id(n.get("sender_id"))) {
                (Some(own), Some(peer)) => own.cmp(&peer),
                _ => std::cmp::Ordering::Equal,
            },
        });
    }
}

/// A published `sender_id` (16 lowercase hex digits, the sender's nonce prefix
/// big-endian) as an integer that orders the same way the prefix bytes do.
/// Anything else is unknown.
fn sender_id(v: Option<&Value>) -> Option<u64> {
    let s = v?.as_str()?;
    if s.len() != 16 {
        return None;
    }
    u64::from_str_radix(s, 16).ok()
}

/// Whether a beacon's reported position is a MEASUREMENT rather than the
/// producer's no-fix placeholder.
///
/// A fleet member with no usable GPS still beacons: the vehicle half fills
/// lat/lon/alt with zeros and leaves the `gps_ok` bit clear rather than
/// withholding the row. Absence is already skipped above; a PRESENT zero is the
/// dangerous half, because (0, 0, 0) is a real geodetic point — thousands of
/// kilometres from any operating site — and the one law with no range bound of
/// its own, `formation::anchor_position(Centroid)`, averages it straight into
/// the fleet anchor. One member sitting on the ground without a lock would
/// otherwise move every armed drone's anchor by `distance / (N + 1)` metres and
/// saturate the commanded velocity toward it.
///
/// The `gps_ok` bit is the primary test: a member that says its fix is bad is
/// telling us its position is not a measurement, whatever numbers it carried.
/// The rest catch a producer that raises the bit anyway.
fn is_measured_fix(lat_deg: f64, lon_deg: f64, alt_m: f64, status: u8) -> bool {
    if status & crate::neighbor::STATUS_GPS_OK == 0 {
        return false;
    }
    if !lat_deg.is_finite() || !lon_deg.is_finite() || !alt_m.is_finite() {
        return false;
    }
    // Null island is the placeholder, never a site. Altitude is deliberately
    // not part of the test: zero AGL is a legitimate reading for an aircraft on
    // the ground, and a producer that zeroes only the position is the same
    // failure.
    if lat_deg == 0.0 && lon_deg == 0.0 {
        return false;
    }
    lat_deg.abs() <= 90.0 && lon_deg.abs() <= 180.0
}

/// Rebuild the beacon status byte from the payload's decoded booleans.
///
/// The payload publishes the flags decoded rather than raw, so the byte is
/// reassembled here. Bits 5..7 come back through `ModePrecedence`, which is what
/// makes a neighbour's ACTIVE level — not its commanded mode — available to this
/// drone's own arbitration and to the operator screen.
fn status_byte(n: &Value) -> u8 {
    let flag = |key: &str, bit: u8| {
        if n.get(key).and_then(Value::as_bool).unwrap_or(false) {
            bit
        } else {
            0
        }
    };
    let mut status = flag("armed", crate::neighbor::STATUS_ARMED)
        | flag("guided", crate::neighbor::STATUS_GUIDED)
        | flag("emergency", crate::neighbor::STATUS_EMERGENCY)
        | flag("gps_ok", crate::neighbor::STATUS_GPS_OK)
        | flag("hero", crate::neighbor::STATUS_HERO);
    if let Some(level) = n.get("mode_precedence").and_then(Value::as_str) {
        status |= crate::ModePrecedence::from_wire(level).as_status_bits();
    }
    status
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::neighbor::{STATUS_ARMED, STATUS_EMERGENCY, STATUS_GUIDED};
    use crate::ModePrecedence;
    use serde_json::json;

    /// A payload in exactly the shape `ados_swarmbus::publish::neighbors_payload`
    /// emits. Every key name here is the contract.
    fn payload(age_ms: u64, vx: f64) -> Value {
        json!({
            "fleet_id": 1,
            "slot": 1,
            "neighbors": [{
                "slot": 3,
                "device_id": "example-drone",
                "seq_ms": 1234,
                "lat": 12.9716,
                "lon": 77.5946,
                "alt_m": 30.0,
                "vx_ms": vx,
                "vy_ms": 0.0,
                "vz_ms": 1.0,
                "heading_deg": 0.0,
                "armed": true,
                "guided": true,
                "emergency": false,
                "gps_ok": true,
                "hero": false,
                "mode_precedence": "formation",
                "age_ms": age_ms,
                "rssi_dbm": -48,
            }],
            "counters": {},
        })
    }

    #[test]
    fn the_published_contract_decodes_to_a_usable_fix() {
        let mut out = Vec::new();
        fixes_from_payload(&payload(0, 5.0), Duration::ZERO, &mut out);
        assert_eq!(out.len(), 1);
        let f = out[0];
        assert_eq!(f.slot, 3);
        assert!((f.lat_deg - 12.9716).abs() < 1e-9);
        assert!((f.vn - 5.0).abs() < 1e-9);
        assert!(
            (f.vd - 1.0).abs() < 1e-9,
            "a descending neighbour reads POSITIVE vd: {f:?}"
        );
        assert_eq!(f.status & STATUS_ARMED, STATUS_ARMED);
        assert_eq!(f.status & STATUS_GUIDED, STATUS_GUIDED);
        assert_eq!(f.status & STATUS_EMERGENCY, 0);
        assert_eq!(
            ModePrecedence::from_status_bits(f.status),
            ModePrecedence::Formation,
            "a neighbour's ACTIVE level has to survive the seam"
        );
    }

    /// The tie-break key crosses the seam from the real producer: each of a
    /// same-slot pair publishes its own sender id and the other's, and the fixes
    /// carry opposite orders, so the pair resolves to exactly one climber.
    #[test]
    fn a_same_slot_pair_reads_opposite_sender_orders_off_the_published_payload() {
        use ados_swarmbus::beacon::{SwarmBeacon, STATUS_GPS_OK};
        use ados_swarmbus::crypto::SenderNonce;
        use ados_swarmbus::neighbors::NeighborTable;
        use ados_swarmbus::publish::neighbors_payload;
        use std::collections::BTreeMap;
        use std::time::Instant;

        let now = Instant::now();
        let a = [0x10u8; 8];
        let b = [0x20u8; 8];
        let order_seen_by = |own: [u8; 8], peer: [u8; 8]| {
            let mut table = NeighborTable::new(5);
            table.set_own_sender(own);
            let beacon = SwarmBeacon {
                slot: 5,
                lat: 129_716_000,
                lon: 775_946_000,
                alt_dm: 300,
                status: STATUS_ARMED | STATUS_GPS_OK,
                ..SwarmBeacon::default()
            };
            let sender = SenderNonce {
                prefix: peer,
                counter: 0,
            };
            table.record(beacon, sender, None, now);
            let p = neighbors_payload(1, &table, &BTreeMap::new(), now);
            let mut out = Vec::new();
            fixes_from_payload(&p, Duration::ZERO, &mut out);
            assert_eq!(out.len(), 1);
            out[0].sender_order
        };
        assert_eq!(order_seen_by(a, b), std::cmp::Ordering::Less);
        assert_eq!(order_seen_by(b, a), std::cmp::Ordering::Greater);

        // No own id published yet: unknown, never a guess.
        let mut out = Vec::new();
        fixes_from_payload(&payload(0, 0.0), Duration::ZERO, &mut out);
        assert_eq!(out[0].sender_order, std::cmp::Ordering::Equal);
    }

    #[test]
    fn the_time_since_the_payload_arrived_is_added_to_the_reported_age() {
        // The 2 Hz feed feeding a 10 Hz loop. Between publishes the only thing that
        // advances is the local elapsed time, so it has to be part of the
        // prediction or four ticks in five replay a frozen frame.
        let mut at_publish = Vec::new();
        fixes_from_payload(&payload(0, 10.0), Duration::ZERO, &mut at_publish);
        let mut mid_interval = Vec::new();
        fixes_from_payload(
            &payload(0, 10.0),
            Duration::from_millis(300),
            &mut mid_interval,
        );
        let advanced = (mid_interval[0].lat_deg - at_publish[0].lat_deg) * crate::geo::DEG_TO_M;
        assert!((advanced - 3.0).abs() < 0.01, "advanced {advanced} m");
        // And it counts toward staleness too, so a socket that stopped delivering
        // ages out on the local clock rather than on a frozen `age_ms`.
        let mut out = Vec::new();
        fixes_from_payload(&payload(100, 0.0), NEIGHBOR_STALE, &mut out);
        assert!(out.is_empty(), "a frozen payload must age out locally");
    }

    #[test]
    fn a_fix_is_dead_reckoned_forward_by_its_reported_age() {
        let mut fresh = Vec::new();
        fixes_from_payload(&payload(0, 10.0), Duration::ZERO, &mut fresh);
        let mut aged = Vec::new();
        fixes_from_payload(&payload(400, 10.0), Duration::ZERO, &mut aged);
        // 0.4 s at 10 m/s north is 4 m of latitude.
        let advanced = (aged[0].lat_deg - fresh[0].lat_deg) * crate::geo::DEG_TO_M;
        assert!((advanced - 4.0).abs() < 0.01, "advanced {advanced} m");
        // The reported velocity is passed through unchanged; only the position is
        // predicted.
        assert!((aged[0].vn - 10.0).abs() < 1e-9);
    }

    #[test]
    fn a_stale_entry_is_dropped_even_if_the_publisher_left_it_in() {
        let mut out = Vec::new();
        let stale = NEIGHBOR_STALE.as_millis() as u64;
        fixes_from_payload(&payload(stale - 1, 0.0), Duration::ZERO, &mut out);
        assert_eq!(out.len(), 1, "just inside the window is still usable");
        fixes_from_payload(&payload(stale, 0.0), Duration::ZERO, &mut out);
        assert!(
            out.is_empty(),
            "a socket that stopped delivering must not leave the layer replaying \
             the last snapshot forever"
        );
    }

    #[test]
    fn an_entry_missing_its_position_is_skipped_not_placed_at_the_origin() {
        let mut out = Vec::new();
        for missing in ["lat", "lon", "alt_m", "slot"] {
            let mut p = payload(0, 0.0);
            p["neighbors"][0]
                .as_object_mut()
                .expect("object")
                .remove(missing);
            fixes_from_payload(&p, Duration::ZERO, &mut out);
            assert!(
                out.is_empty(),
                "a neighbour with no {missing} is not a neighbour at the equator"
            );
        }
        // A missing VELOCITY is different: zero is the honest reading for a
        // hovering aircraft and the position is still known.
        let mut p = payload(0, 0.0);
        let obj = p["neighbors"][0].as_object_mut().expect("object");
        obj.remove("vx_ms");
        obj.remove("vy_ms");
        obj.remove("vz_ms");
        fixes_from_payload(&p, Duration::ZERO, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].vn, out[0].ve, out[0].vd), (0.0, 0.0, 0.0));
    }

    #[test]
    fn a_zero_position_is_skipped_even_though_every_field_is_present() {
        // What a member that booted without a lock actually beacons: the vehicle
        // half zeroes the position rather than withholding the row. Present-and-
        // zero is not the same shape as absent, and only the absent case was
        // being refused.
        let mut out = Vec::new();
        let mut p = payload(0, 0.0);
        let obj = p["neighbors"][0].as_object_mut().expect("object");
        obj.insert("lat".into(), json!(0.0));
        obj.insert("lon".into(), json!(0.0));
        obj.insert("alt_m".into(), json!(0.0));
        fixes_from_payload(&p, Duration::ZERO, &mut out);
        assert!(
            out.is_empty(),
            "a member with no fix is not a member at the equator"
        );

        // Even with gps_ok raised: a producer that zeroes the position and still
        // claims a lock is the same phantom aircraft.
        let obj = p["neighbors"][0].as_object_mut().expect("object");
        obj.insert("gps_ok".into(), json!(true));
        fixes_from_payload(&p, Duration::ZERO, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn a_cleared_gps_ok_bit_skips_the_row_whatever_position_it_carried() {
        // A member reporting its own fix as bad is telling us its position is
        // not a measurement. Flying geometry against it fabricates a lattice.
        let mut out = Vec::new();
        let mut p = payload(0, 0.0);
        p["neighbors"][0]
            .as_object_mut()
            .expect("object")
            .insert("gps_ok".into(), json!(false));
        fixes_from_payload(&p, Duration::ZERO, &mut out);
        assert!(out.is_empty(), "no lock means no usable position");

        // Absent reads the same as false — the payload publishes the bit
        // decoded, so a producer that dropped it is not asserting a fix.
        let mut p = payload(0, 0.0);
        p["neighbors"][0]
            .as_object_mut()
            .expect("object")
            .remove("gps_ok");
        fixes_from_payload(&p, Duration::ZERO, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn a_position_outside_the_geodetic_range_is_skipped() {
        // A garbage number is not a position. `json!` folds NaN and infinity to
        // null (which the missing-field path already refuses), so the reachable
        // shape of the same fault is a finite value out of range.
        let mut out = Vec::new();
        for (lat, lon) in [
            (91.0, 10.0),
            (-91.0, 10.0),
            (10.0, 181.0),
            (10.0, -181.0),
            (f64::MAX, 10.0),
            (10.0, -f64::MAX),
        ] {
            let mut p = payload(0, 0.0);
            let obj = p["neighbors"][0].as_object_mut().expect("object");
            obj.insert("lat".into(), json!(lat));
            obj.insert("lon".into(), json!(lon));
            fixes_from_payload(&p, Duration::ZERO, &mut out);
            assert!(out.is_empty(), "({lat}, {lon}) is not a position");
        }
    }

    #[test]
    fn a_malformed_or_empty_payload_yields_no_fixes_and_does_not_panic() {
        let mut out = vec![];
        for p in [
            json!({}),
            json!({ "neighbors": [] }),
            json!({ "neighbors": {} }),
            json!({ "neighbors": [null, 7, "x"] }),
            json!(null),
        ] {
            fixes_from_payload(&p, Duration::ZERO, &mut out);
            assert!(out.is_empty(), "{p:?}");
        }
    }

    #[test]
    fn the_scratch_buffer_is_reused_not_appended_to() {
        let mut out = Vec::new();
        fixes_from_payload(&payload(0, 0.0), Duration::ZERO, &mut out);
        fixes_from_payload(&payload(0, 0.0), Duration::ZERO, &mut out);
        assert_eq!(
            out.len(),
            1,
            "a 10 Hz loop must not grow the buffer forever"
        );
    }

    #[test]
    fn the_outbound_extra_keys_are_the_ones_the_router_publishes() {
        // Pinned because `ados_swarmbus::vehicle::beacon_from_state` reads them by
        // name out of the state snapshot; a rename on either side is silent.
        assert_eq!(EXTRA_PRECEDENCE, "swarm_precedence");
        assert_eq!(EXTRA_EMERGENCY, "swarm_emergency");
    }
}
