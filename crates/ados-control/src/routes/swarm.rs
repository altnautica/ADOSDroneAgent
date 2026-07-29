//! The swarm neighbour-table read.
//!
//! `GET /api/swarm/neighbors` serves the table `ados-swarmbus` publishes on
//! `/run/ados/swarm.sock`: every node in this fleet that this node currently hears,
//! with its position, velocity, condition bits, active mode-precedence level, beacon
//! age and signal.
//!
//! **Profile-agnostic on purpose.** The same route answers on a drone and on a
//! ground station, and that is the whole decentralization proof: query a drone with
//! the ground station powered off and it still lists every other drone. A route that
//! only existed on the ground station would be a fan-out, not a bus.
//!
//! Guaranteed 200. When the bus has published nothing — an absent socket, a profile
//! that does not run it, a radio that has not come up — the body is the degraded
//! shape from [`ados_swarmbus::publish::empty_payload`]: an empty neighbour array,
//! zeroed counters, and `fleet_id`/`slot` as **null**. Null rather than the config
//! defaults, because a reader cannot know the fleet identity of a service that is
//! not running, and reporting `1`/`0` would make an unprovisioned node
//! indistinguishable from a correctly-provisioned fleet-1 node with no neighbours.
//!
//! The shape is normalised through the producer crate rather than re-projected here,
//! so a published payload from a skewed agent build still answers for every key
//! Mission Control's beacon store requires.

use ados_swarmbus::publish::normalise_payload;
use axum::extract::State;
use axum::Json;
use serde_json::Value;

use crate::state::AppState;

/// `GET /api/swarm/neighbors` → the fleet's neighbour table.
pub async fn get_neighbors(State(state): State<AppState>) -> Json<Value> {
    Json(normalise_payload(state.swarm.published().as_ref()))
}

/// The `neighbors` array for `/api/status/full`, or `None` when the swarm bus has
/// published nothing.
///
/// `None` omits the key rather than emitting `[]`, matching how the consolidated
/// route treats every other optional block (`crsf`, `linked_peers`): an empty array
/// would claim "this node hears no neighbours", which is a different fact from "this
/// node is not running a swarm bus". A client that needs the counters or wants the
/// degraded shape explicitly reads the dedicated route, which is guaranteed 200.
pub fn neighbors_for_full_status(state: &AppState) -> Option<Value> {
    let published = state.swarm.published()?;
    Some(normalise_payload(Some(&published))["neighbors"].clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_swarmbus::publish::{COUNTER_KEYS, NEIGHBOR_KEYS};
    use serde_json::json;
    use std::path::Path;
    use std::sync::Arc;

    use crate::auth::PairingState;
    use crate::ipc::{LogdQueryClient, MavlinkIpcClient, StateIpcClient};
    use crate::state::PairingPaths;

    /// An `AppState` for a handler test: a disconnected swarm client (the test primes
    /// it directly) and inert paths/clients for everything else. Mirrors the harness
    /// the params-route tests use.
    fn test_state(dir: &Path) -> AppState {
        let pairing_paths = PairingPaths {
            config: dir.join("config.yaml"),
            pairing_json: dir.join("pairing.json"),
            wfb_key_dir: dir.join("wfb"),
            bind_state: dir.join("bind-state.json"),
            profile_conf: dir.join("profile.conf"),
            mesh_role: dir.join("mesh-role"),
        };
        AppState::new(
            Arc::new(PairingState::with_path(dir.join("pairing.json"))),
            StateIpcClient::disconnected(),
            MavlinkIpcClient::new(dir.join("absent-mavlink.sock")),
            LogdQueryClient::new(dir.join("absent-logd.sock")),
            dir.join("board.json"),
            pairing_paths,
            Arc::new(crate::dashboard_pin::DashboardPin::with_path(
                dir.join("dashboard-pin.json"),
            )),
            Arc::new(crate::mcp::McpTokenStore::with_path(
                dir.join("mcp-token.json"),
            )),
        )
    }

    /// A live payload, in the exact shape the bus publishes.
    fn published() -> Value {
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
                "heading_deg": 341.6,
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
                "beacons_rx": 11,
                "beacons_bad_magic": 0,
                "beacons_bad_tag": 0,
                "beacons_stale_dropped": 0,
                "neighbors_now": 1,
            },
            "slots": [],
        })
    }

    /// A published table is served through verbatim. Mission Control is typed against
    /// this body, so the route must not reshape it.
    #[tokio::test]
    async fn a_published_table_is_served_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        state.swarm.set_for_test(published());
        let Json(got) = get_neighbors(State(state)).await;
        assert_eq!(got, published());
        // Every contract key is present on the row and in the counters.
        for k in NEIGHBOR_KEYS {
            assert!(got["neighbors"][0].get(k).is_some(), "row is missing {k}");
        }
        for k in COUNTER_KEYS {
            assert!(got["counters"].get(k).is_some(), "counters missing {k}");
        }
    }

    /// With no bus running the body is the degraded shape — structurally complete, so
    /// a client parses it, and null-identified, so it does not claim a fleet. The
    /// route is guaranteed 200 on every profile, so this is a real answer, not an
    /// error path.
    #[tokio::test]
    async fn no_published_table_yields_the_degraded_body_not_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let Json(got) = get_neighbors(State(test_state(dir.path()))).await;
        assert_eq!(got["fleet_id"], Value::Null, "a guessed fleet id is a lie");
        assert_eq!(got["slot"], Value::Null);
        assert_eq!(got["neighbors"], json!([]));
        for k in COUNTER_KEYS {
            assert_eq!(got["counters"][k], json!(0), "{k} must be zero, not absent");
        }
    }

    /// The consolidated route omits the key when the bus is silent, because an empty
    /// array would claim "no neighbours heard" — a different fact from "no bus".
    #[test]
    fn the_full_status_block_is_omitted_when_the_bus_is_silent() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        assert!(neighbors_for_full_status(&state).is_none());

        state.swarm.set_for_test(published());
        let block = neighbors_for_full_status(&state).expect("a published table folds in");
        assert_eq!(block, published()["neighbors"]);
        assert_eq!(block.as_array().unwrap().len(), 1);
    }

    /// A genuinely empty table from a RUNNING bus is `[]`, which is a real answer and
    /// must be folded in rather than omitted — that is the distinction the omit rule
    /// above exists to preserve.
    #[tokio::test]
    async fn a_running_bus_with_no_neighbours_folds_in_an_empty_array() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        state.swarm.set_for_test(json!({
            "fleet_id": 1,
            "slot": 2,
            "neighbors": [],
            "counters": {"beacons_tx": 40, "neighbors_now": 0},
        }));
        assert_eq!(neighbors_for_full_status(&state), Some(json!([])));
        // And the dedicated route reports the real identity and counters.
        let Json(got) = get_neighbors(State(state)).await;
        assert_eq!(got["fleet_id"], json!(1));
        assert_eq!(got["slot"], json!(2));
        assert_eq!(got["counters"]["beacons_tx"], json!(40));
    }

    /// A payload from a skewed agent build must still answer for every key the GCS
    /// store requires, rather than being passed through with keys missing.
    #[tokio::test]
    async fn a_partial_payload_is_normalised_to_the_full_contract() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path());
        state
            .swarm
            .set_for_test(json!({"fleet_id": 9, "neighbors": [{"slot": 4}]}));
        let Json(got) = get_neighbors(State(state)).await;
        assert_eq!(got["fleet_id"], json!(9));
        assert_eq!(got["slot"], Value::Null, "omitted by the producer");
        for k in COUNTER_KEYS {
            assert!(got["counters"].get(k).is_some(), "counters missing {k}");
        }
    }

    /// The route must be reachable on a DRONE profile, not just a ground station.
    /// That is the decentralization property the whole bus exists for: query a drone
    /// with the ground station off and it still lists every other drone. A
    /// profile-gated 404 here would silently turn the bus into a fan-out.
    #[tokio::test]
    async fn the_route_is_profile_agnostic() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.yaml"), "agent:\n  profile: drone\n").unwrap();
        let state = test_state(dir.path());
        state.swarm.set_for_test(json!({
            "fleet_id": 1,
            "slot": 5,
            "neighbors": [{"slot": 3}, {"slot": 9}],
            "counters": {"neighbors_now": 2},
        }));
        let Json(got) = get_neighbors(State(state)).await;
        assert_eq!(got["slot"], json!(5), "the drone reports its own slot");
        assert_eq!(got["neighbors"].as_array().unwrap().len(), 2);
    }
}
