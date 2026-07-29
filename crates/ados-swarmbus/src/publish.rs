//! The neighbour-table payload published on `/run/ados/swarm.sock`.
//!
//! One JSON object per publish, newline-terminated, at the beacon rate. Both
//! consumers read the same bytes: `ados-control` serves them verbatim on
//! `GET /api/swarm/neighbors`, and Mission Control's beacon store is typed against
//! exactly this shape. That makes [`neighbors_payload`] a **published contract**,
//! not an internal detail — the tests below pin every key name and every null
//! rule, because a rename here is a silent break in the operator's fleet view.
//!
//! Two rules the shape follows throughout:
//!
//! - **A missing reading is `null`, never a plausible number.** `rssi_dbm` is null
//!   when the capture carried no signal field and `device_id` is null when the
//!   slot cannot be joined to an identity. A fabricated `-100 dBm` or an empty
//!   string would render as a real value.
//! - **Derived fields are computed here, not on the client.** `heading_deg` and
//!   `age_ms` are emitted so every consumer agrees on them; two clients deriving
//!   heading with different argument orders would mirror the map.
//! - **`neighbors` is who is heard; `slots` is who is registered.** The
//!   `neighbors` array holds only slots this node's radio has actually received a
//!   beacon from since it started; `slots` holds the fleet's whole registered-slot
//!   table (slot → device id) regardless of whether that slot is currently
//!   beaconing. The two differ exactly when a drone is lost, which is the
//!   operator-facing fact `slots` exists to carry — a slot present in `slots` but
//!   absent from `neighbors` is a drone the fleet issued a slot to and has since
//!   stopped hearing.

use std::collections::BTreeMap;
use std::time::Instant;

use serde_json::{json, Value};

use crate::neighbors::{NeighborTable, SwarmCounters};

/// Build the payload published on the swarm socket.
///
/// `device_ids` joins slot to identity. It is deliberately an argument rather than
/// state on the table: the table is fed by the radio and knows only slots, while
/// identities come from the ground station's fleet registry, and pushing the join
/// into the table would give it a second, slower-moving source of truth.
pub fn neighbors_payload(
    fleet_id: u16,
    table: &NeighborTable,
    device_ids: &BTreeMap<u8, String>,
    now: Instant,
) -> Value {
    let neighbors: Vec<Value> = table
        .iter()
        .map(|(slot, n)| {
            let b = &n.beacon;
            json!({
                "slot": slot,
                "device_id": device_ids.get(slot).map(String::as_str),
                "seq_ms": b.seq_ms,
                "lat": b.lat_deg(),
                "lon": b.lon_deg(),
                "alt_m": b.alt_m(),
                "vx_ms": b.vx_ms(),
                "vy_ms": b.vy_ms(),
                "vz_ms": b.vz_ms(),
                "heading_deg": b.heading_deg(),
                "armed": b.armed(),
                "guided": b.guided(),
                "emergency": b.emergency(),
                "gps_ok": b.gps_ok(),
                "hero": b.hero(),
                "mode_precedence": b.precedence().as_wire(),
                "age_ms": n.age(now).as_millis(),
                "rssi_dbm": n.rssi_dbm,
            })
        })
        .collect();

    json!({
        "fleet_id": fleet_id,
        "slot": table.own_slot(),
        "neighbors": neighbors,
        "counters": counters_value(table.counters(), table.len()),
        "slots": device_ids
            .iter()
            .map(|(slot, device_id)| json!({"slot": slot, "device_id": device_id}))
            .collect::<Vec<Value>>(),
    })
}

/// The counter block. `neighbors_now` is derived from the live table rather than
/// stored, so it can never disagree with the array beside it.
pub fn counters_value(c: SwarmCounters, neighbors_now: usize) -> Value {
    json!({
        "beacons_tx": c.beacons_tx,
        "beacons_rx": c.beacons_rx,
        "beacons_bad_magic": c.beacons_bad_magic,
        "beacons_bad_tag": c.beacons_bad_tag,
        "beacons_stale_dropped": c.beacons_stale_dropped,
        "neighbors_now": neighbors_now,
    })
}

/// The degraded body a consumer serves when the swarm service has published
/// nothing — an absent socket, or a bus that has not opened its radio yet.
///
/// `fleet_id` and `slot` are **null**, not `1` and `0`. Those would be a guess: a
/// reader cannot know the fleet identity of a service that is not running, and
/// reporting the defaults would make an unprovisioned node indistinguishable from a
/// correctly-provisioned fleet-1 node with no neighbours. The counters are all
/// zero, which is honest — nothing has been transmitted or received. `slots` is
/// an empty array rather than null: an empty registry is an honest description of
/// a node with no registry of its own (every drone, and a ground station that has
/// paired nobody), whereas `fleet_id`/`slot` being empty would be a guess about a
/// fleet identity this degraded body cannot know.
pub fn empty_payload() -> Value {
    json!({
        "fleet_id": Value::Null,
        "slot": Value::Null,
        "neighbors": [],
        "counters": counters_value(SwarmCounters::default(), 0),
        "slots": [],
    })
}

/// Normalise an arbitrary published value into the contract shape, filling any key
/// the producer omitted.
///
/// The seam that keeps the HTTP surface's contract stable across an agent-version
/// skew: a payload from an older or newer swarm service is served with its own
/// values for the keys it carries and the degraded defaults for the rest, rather
/// than passed through with keys the GCS store requires simply missing.
pub fn normalise_payload(published: Option<&Value>) -> Value {
    let Some(Value::Object(src)) = published else {
        return empty_payload();
    };
    let Value::Object(mut out) = empty_payload() else {
        unreachable!("empty_payload is an object")
    };
    for key in ["fleet_id", "slot", "neighbors", "counters", "slots"] {
        if let Some(v) = src.get(key) {
            out.insert(key.to_string(), v.clone());
        }
    }
    // A counter block from a producer that predates a counter still answers for
    // every key the contract names.
    if let Some(Value::Object(counters)) = out.get("counters").cloned() {
        let Value::Object(mut filled) = counters_value(SwarmCounters::default(), 0) else {
            unreachable!("counters_value is an object")
        };
        for (k, v) in counters {
            filled.insert(k, v);
        }
        out.insert("counters".to_string(), Value::Object(filled));
    }
    Value::Object(out)
}

/// Encode a payload as one newline-terminated JSON line, the swarm socket's frame.
pub fn encode_line(payload: &Value) -> Vec<u8> {
    let mut buf = serde_json::to_vec(payload).unwrap_or_else(|_| b"{}".to_vec());
    buf.push(b'\n');
    buf
}

/// The keys the contract requires on a neighbour row. Exported so the shape is
/// asserted from one list rather than a hand-copied one per test.
pub const NEIGHBOR_KEYS: [&str; 18] = [
    "slot",
    "device_id",
    "seq_ms",
    "lat",
    "lon",
    "alt_m",
    "vx_ms",
    "vy_ms",
    "vz_ms",
    "heading_deg",
    "armed",
    "guided",
    "emergency",
    "gps_ok",
    "hero",
    "mode_precedence",
    "age_ms",
    "rssi_dbm",
];

/// The keys the contract requires in the counter block.
pub const COUNTER_KEYS: [&str; 6] = [
    "beacons_tx",
    "beacons_rx",
    "beacons_bad_magic",
    "beacons_bad_tag",
    "beacons_stale_dropped",
    "neighbors_now",
];

/// The keys the contract requires on a slot-table row. Exported so the shape is
/// asserted from one list rather than a hand-copied one per test, same as
/// [`NEIGHBOR_KEYS`] and [`COUNTER_KEYS`].
pub const SLOT_KEYS: [&str; 2] = ["slot", "device_id"];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::beacon::{SwarmBeacon, STATUS_ARMED, STATUS_GPS_OK, STATUS_GUIDED, STATUS_HERO};
    use crate::ModePrecedence;
    use serde_json::Map;
    use std::time::Duration;

    /// Assert a value carries exactly `keys` and nothing else — so an ADDED key
    /// fails too. A silently-added key is how a published contract rots.
    fn assert_exact_keys(v: &Value, keys: &[&str], what: &str) {
        let obj: &Map<String, Value> = v.as_object().unwrap_or_else(|| panic!("{what} object"));
        let got: std::collections::BTreeSet<&str> = obj.keys().map(String::as_str).collect();
        let want: std::collections::BTreeSet<&str> = keys.iter().copied().collect();
        assert_eq!(got, want, "{what} key set drifted from the contract");
    }

    fn table_with_one(now: Instant) -> NeighborTable {
        let mut t = NeighborTable::new(0);
        t.record(
            SwarmBeacon {
                slot: 3,
                seq_ms: 41234,
                lat: 129_716_000,
                lon: 775_946_000,
                alt_dm: 325,
                vx_cms: 120,
                vy_cms: -40,
                vz_cms: 0,
                status: STATUS_ARMED | STATUS_GUIDED | STATUS_GPS_OK,
            },
            Some(-48),
            now,
        );
        t
    }

    fn ids() -> BTreeMap<u8, String> {
        BTreeMap::from([(3u8, "ados-abc123".to_string())])
    }

    /// The exact body the batch contract specifies, value for value. Mission
    /// Control's store is typed against this, so a drift here breaks the operator's
    /// fleet view with no compile error anywhere.
    #[test]
    fn the_payload_matches_the_published_contract_value_for_value() {
        let t0 = Instant::now();
        let table = table_with_one(t0);
        let got = neighbors_payload(1, &table, &ids(), t0 + Duration::from_millis(420));
        assert_eq!(
            got,
            json!({
                "fleet_id": 1,
                "slot": 0,
                "neighbors": [{
                    "slot": 3,
                    "device_id": "ados-abc123",
                    "seq_ms": 41234,
                    "lat": 12.9716,
                    "lon": 77.5946,
                    "alt_m": 32.5,
                    "vx_ms": 1.2,
                    "vy_ms": -0.4,
                    "vz_ms": 0.0,
                    "heading_deg": 341.565051177078,
                    "armed": true,
                    "guided": true,
                    "emergency": false,
                    "gps_ok": true,
                    "hero": false,
                    "mode_precedence": "hold",
                    "age_ms": 420,
                    "rssi_dbm": -48,
                }],
                "counters": {
                    "beacons_tx": 0,
                    "beacons_rx": 1,
                    "beacons_bad_magic": 0,
                    "beacons_bad_tag": 0,
                    "beacons_stale_dropped": 0,
                    "neighbors_now": 1,
                },
                "slots": [{"slot": 3, "device_id": "ados-abc123"}],
            })
        );
    }

    /// The key sets are pinned as sets, so an ADDED key fails too — a consumer
    /// asserting an exact shape would break on one, and a silently-added key is how
    /// a contract rots.
    #[test]
    fn the_key_sets_are_exactly_the_contract_and_carry_nothing_extra() {
        let t0 = Instant::now();
        let payload = neighbors_payload(1, &table_with_one(t0), &ids(), t0);
        assert_exact_keys(
            &payload,
            &["fleet_id", "slot", "neighbors", "counters", "slots"],
            "payload",
        );
        assert_exact_keys(&payload["neighbors"][0], &NEIGHBOR_KEYS, "neighbor row");
        assert_exact_keys(&payload["counters"], &COUNTER_KEYS, "counters");
        assert_exact_keys(&payload["slots"][0], &SLOT_KEYS, "slot row");
    }

    /// A missing reading must be `null`. A fabricated `-100 dBm` or `""` would
    /// render as a real measurement in the operator's signal and name columns.
    #[test]
    fn an_unknown_signal_or_identity_is_null_not_a_plausible_value() {
        let t0 = Instant::now();
        let mut table = NeighborTable::new(1);
        table.record(
            SwarmBeacon {
                slot: 4,
                ..SwarmBeacon::default()
            },
            None,
            t0,
        );
        let row = &neighbors_payload(1, &table, &BTreeMap::new(), t0)["neighbors"][0];
        assert_eq!(row["rssi_dbm"], Value::Null);
        assert_eq!(row["device_id"], Value::Null);
        // Present-but-null is not the same as absent; the keys must still be there.
        assert!(row.as_object().unwrap().contains_key("rssi_dbm"));
        assert!(row.as_object().unwrap().contains_key("device_id"));
    }

    #[test]
    fn rows_are_slot_ordered_and_neighbors_now_matches_the_array() {
        let t0 = Instant::now();
        let mut table = NeighborTable::new(1);
        for slot in [9u8, 2, 24, 5] {
            table.record(
                SwarmBeacon {
                    slot,
                    ..SwarmBeacon::default()
                },
                None,
                t0,
            );
        }
        let p = neighbors_payload(1, &table, &BTreeMap::new(), t0);
        let slots: Vec<u64> = p["neighbors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["slot"].as_u64().unwrap())
            .collect();
        assert_eq!(slots, vec![2, 5, 9, 24]);
        assert_eq!(p["counters"]["neighbors_now"], json!(4));
        assert_eq!(p["neighbors"].as_array().unwrap().len(), 4);
    }

    /// `age_ms` is what the operator surface renders as beacon age and what it
    /// times its own staleness fade against, so it must track elapsed time rather
    /// than being a constant or the beacon's own `seq_ms`.
    #[test]
    fn age_ms_tracks_elapsed_time_since_receipt() {
        let t0 = Instant::now();
        let table = table_with_one(t0);
        for ms in [0u64, 1, 499, 1500, 4000] {
            let p = neighbors_payload(1, &table, &ids(), t0 + Duration::from_millis(ms));
            assert_eq!(p["neighbors"][0]["age_ms"], json!(ms));
        }
        // And it is not the beacon's sequence counter.
        let p = neighbors_payload(1, &table, &ids(), t0);
        assert_eq!(p["neighbors"][0]["age_ms"], json!(0));
        assert_eq!(p["neighbors"][0]["seq_ms"], json!(41234));
    }

    /// Every precedence level must serialize to its published spelling, because
    /// Mission Control types the field as a closed union.
    #[test]
    fn every_precedence_level_serializes_to_its_published_spelling() {
        let t0 = Instant::now();
        for level in ModePrecedence::ARBITRATION_ORDER {
            let mut table = NeighborTable::new(1);
            let mut b = SwarmBeacon {
                slot: 2,
                ..SwarmBeacon::default()
            };
            b.set_precedence(level);
            table.record(b, None, t0);
            let p = neighbors_payload(1, &table, &BTreeMap::new(), t0);
            assert_eq!(
                p["neighbors"][0]["mode_precedence"],
                json!(level.as_wire()),
                "{level:?}"
            );
        }
    }

    #[test]
    fn each_status_bit_surfaces_as_its_own_boolean() {
        let t0 = Instant::now();
        let row_for = |status: u8| {
            let mut table = NeighborTable::new(1);
            table.record(
                SwarmBeacon {
                    slot: 2,
                    status,
                    ..SwarmBeacon::default()
                },
                None,
                t0,
            );
            neighbors_payload(1, &table, &BTreeMap::new(), t0)["neighbors"][0].clone()
        };
        let hero = row_for(STATUS_HERO);
        assert_eq!(hero["hero"], json!(true));
        assert_eq!(hero["armed"], json!(false));
        assert_eq!(hero["guided"], json!(false));
        assert_eq!(hero["gps_ok"], json!(false));
        assert_eq!(hero["emergency"], json!(false));
        let armed = row_for(STATUS_ARMED);
        assert_eq!(armed["armed"], json!(true));
        assert_eq!(armed["hero"], json!(false));
    }

    /// The degraded body must be honest about not knowing the fleet identity, and
    /// must still carry every key the consumer's type requires.
    #[test]
    fn the_degraded_body_is_null_identified_and_structurally_complete() {
        let p = empty_payload();
        assert_eq!(p["fleet_id"], Value::Null, "a guessed fleet id is a lie");
        assert_eq!(p["slot"], Value::Null);
        assert_eq!(p["neighbors"], json!([]));
        assert_eq!(p["slots"], json!([]));
        assert_exact_keys(
            &p,
            &["fleet_id", "slot", "neighbors", "counters", "slots"],
            "payload",
        );
        assert_exact_keys(&p["counters"], &COUNTER_KEYS, "counters");
        for k in COUNTER_KEYS {
            assert_eq!(p["counters"][k], json!(0), "{k} must be zero, not absent");
        }
    }

    /// Version skew in either direction must still produce the contract shape: a
    /// producer that omits a key gets the degraded default, and one that adds an
    /// unknown key does not smuggle it through.
    #[test]
    fn normalise_fills_omitted_keys_and_drops_unknown_ones() {
        // Nothing published at all.
        assert_eq!(normalise_payload(None), empty_payload());
        assert_eq!(normalise_payload(Some(&json!("garbage"))), empty_payload());
        assert_eq!(normalise_payload(Some(&json!([]))), empty_payload());

        // A producer that carries only some keys, plus one we do not know.
        let partial = json!({
            "fleet_id": 7,
            "neighbors": [{"slot": 2}],
            "counters": {"beacons_rx": 9},
            "surprise": true,
        });
        let got = normalise_payload(Some(&partial));
        assert_eq!(got["fleet_id"], json!(7), "carried through");
        assert_eq!(got["slot"], Value::Null, "omitted, so degraded");
        assert_eq!(got["neighbors"], json!([{"slot": 2}]));
        assert_eq!(got["slots"], json!([]), "omitted, so degraded");
        assert_eq!(got["counters"]["beacons_rx"], json!(9));
        assert_eq!(got["counters"]["beacons_tx"], json!(0), "filled");
        assert_exact_keys(&got["counters"], &COUNTER_KEYS, "counters");
        assert_exact_keys(
            &got,
            &["fleet_id", "slot", "neighbors", "counters", "slots"],
            "payload",
        );

        // A complete payload passes through byte-identically.
        let t0 = Instant::now();
        let full = neighbors_payload(1, &table_with_one(t0), &ids(), t0);
        assert_eq!(normalise_payload(Some(&full)), full);
    }

    #[test]
    fn the_socket_frame_is_one_newline_terminated_json_line() {
        let t0 = Instant::now();
        let payload = neighbors_payload(1, &table_with_one(t0), &ids(), t0);
        let line = encode_line(&payload);
        assert_eq!(*line.last().unwrap(), b'\n');
        assert_eq!(
            line.iter().filter(|b| **b == b'\n').count(),
            1,
            "exactly one newline, or the reader mis-frames"
        );
        let decoded: Value = serde_json::from_slice(&line[..line.len() - 1]).unwrap();
        assert_eq!(decoded, payload);
    }

    /// A slot the fleet registered but has not heard from must still appear in
    /// `slots` — that is the entire point of the key — while `neighbors` carries
    /// only slots this node has actually heard a beacon from.
    #[test]
    fn a_registered_slot_with_no_beacon_appears_in_slots_but_not_neighbors() {
        let t0 = Instant::now();
        let device_ids = BTreeMap::from([
            (3u8, "ados-abc123".to_string()),
            (9u8, "ados-def456".to_string()),
        ]);
        let table = table_with_one(t0); // only slot 3 has ever beaconed
        let payload = neighbors_payload(1, &table, &device_ids, t0);
        assert_eq!(payload["slots"].as_array().unwrap().len(), 2);
        assert_eq!(payload["neighbors"].as_array().unwrap().len(), 1);
    }

    /// The degraded and normalised paths must default `slots` to an empty array,
    /// same as every other array-shaped key.
    #[test]
    fn slots_defaults_to_empty_when_no_registry_is_known() {
        assert_eq!(empty_payload()["slots"], json!([]));
        assert_eq!(normalise_payload(None)["slots"], json!([]));
    }
}
