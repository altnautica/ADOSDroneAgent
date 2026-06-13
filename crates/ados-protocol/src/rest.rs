//! REST client and shared wire types for the agent HTTP API (Contract D, port
//! 8080).
//!
//! The FastAPI REST layer stays Python; this is the client side used by the
//! Rust TUI (and any other Rust REST consumer). Auth is the `X-ADOS-Key`
//! header; the pairing routes are reachable while unpaired. The client is
//! blocking (`ureq`, HTTP-only) because its callers are simple pollers off the
//! flight-critical path.
//!
//! The client returns raw [`serde_json::Value`] bodies. The [`types`] submodule
//! adds typed serde structs for the read-projection shapes that the native
//! route handlers project out of subsystem sockets and sidecars. Defining each
//! shape once here keeps the data contract in the shared protocol crate, so a
//! handler does not re-invent an ad-hoc struct per route. Every type carries an
//! open `extra` map (via `#[serde(flatten)]`) and `#[serde(default)]` fields, so
//! a new field on the producer side round-trips through a consumer that does not
//! yet know about it.

use std::time::Duration;

use serde_json::Value;
use thiserror::Error;

pub use types::*;

/// Default base URL: the agent's local HTTP API. Matches `cli/main.py`'s
/// `API_BASE`.
pub const DEFAULT_BASE: &str = "http://localhost:8080";

/// Default per-request timeout, matching the Python CLI's httpx client.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Debug, Error)]
pub enum RestError {
    #[error("http {0}")]
    Status(u16),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("response was not valid json: {0}")]
    Json(String),
}

/// A blocking REST client for the agent HTTP API.
pub struct RestClient {
    base: String,
    api_key: Option<String>,
    agent: ureq::Agent,
}

impl RestClient {
    /// Build a client for the given base URL (no trailing slash).
    pub fn new(base: impl Into<String>) -> Self {
        let agent = ureq::AgentBuilder::new().timeout(DEFAULT_TIMEOUT).build();
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            api_key: None,
            agent,
        }
    }

    /// Client for the default local agent (`http://localhost:8080`).
    pub fn local() -> Self {
        Self::new(DEFAULT_BASE)
    }

    /// Attach the pairing key sent as `X-ADOS-Key` on every request.
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// GET `path` (e.g. `/api/status`) and parse the JSON body.
    pub fn get_json(&self, path: &str) -> Result<Value, RestError> {
        let url = format!("{}{}", self.base, path);
        let mut req = self.agent.get(&url);
        if let Some(ref key) = self.api_key {
            req = req.set("X-ADOS-Key", key);
        }
        match req.call() {
            Ok(resp) => resp
                .into_json::<Value>()
                .map_err(|e| RestError::Json(e.to_string())),
            Err(ureq::Error::Status(code, _resp)) => Err(RestError::Status(code)),
            Err(ureq::Error::Transport(t)) => Err(RestError::Transport(t.to_string())),
        }
    }

    /// GET `/api/status` — the agent status snapshot the dashboard renders.
    pub fn status(&self) -> Result<Value, RestError> {
        self.get_json("/api/status")
    }

    /// GET `/api/v1/setup/status` — the setup wizard state (read-only).
    pub fn setup_status(&self) -> Result<Value, RestError> {
        self.get_json("/api/v1/setup/status")
    }
}

/// Typed read-projection shapes for the agent HTTP API.
///
/// These structs describe the JSON a native route handler returns when it
/// projects a subsystem IPC seam or sidecar into the HTTP body the ground
/// station already consumes. They are deliberately loose: scalar fields are
/// `Option` with `#[serde(default)]`, and each struct ends in a flattened
/// `extra` map that captures any field a producer adds ahead of the consumer.
/// A value therefore round-trips byte-for-content even when the two ends are on
/// different versions, which is the forward-compatibility contract the rest of
/// this crate keeps for its open `serde_json::Value` maps.
pub mod types {
    use serde::{Deserialize, Serialize};
    use serde_json::{Map, Value};

    /// An open map carrying any JSON fields a consumer does not yet model. Used
    /// as the `#[serde(flatten)]` tail of every projection struct so unknown
    /// fields survive a round-trip instead of being dropped.
    pub type Extra = Map<String, Value>;

    /// One flight-controller parameter as the parameter-list route reports it.
    #[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
    pub struct ParamEntry {
        /// Parameter name (e.g. `WPNAV_SPEED`).
        pub name: String,
        /// Current value. Parameters are carried as floats on the wire; an
        /// integer parameter is the exact float of that integer.
        pub value: f64,
        /// Optional MAVLink parameter type tag (e.g. `INT32`, `REAL32`).
        #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
        pub ptype: Option<String>,
        /// Any extra fields the producer attaches per parameter.
        #[serde(flatten)]
        pub extra: Extra,
    }

    /// The flight-controller parameter list.
    #[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
    pub struct ParamsResponse {
        /// The parameters, in the order the route returns them.
        #[serde(default)]
        pub params: Vec<ParamEntry>,
        /// Any extra envelope fields (counts, completion flags, timestamps).
        #[serde(flatten)]
        pub extra: Extra,
    }

    /// The radio link state projected from the WFB manager's sidecar.
    ///
    /// `status` and `active` are the always-present summary; the numeric link
    /// stats are `Option` because they only exist once a link is up. New
    /// signals (per-stream stats, antenna diversity, fec) ride the `extra` map.
    #[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
    pub struct WfbLinkState {
        /// Coarse link status (e.g. `locked`, `searching`, `down`).
        #[serde(default)]
        pub status: String,
        /// Whether the radio transport is currently running.
        #[serde(default)]
        pub active: bool,
        /// Active WFB channel, when bound.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub channel: Option<u32>,
        /// Received signal strength in dBm, when a peer is heard.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub rssi_dbm: Option<i32>,
        /// Decoded (FEC + decrypt) received packets per second.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub valid_rx_packets_per_s: Option<f64>,
        /// Transmit throughput in bytes per second.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub tx_bytes_per_s: Option<f64>,
        /// Any extra link fields the manager surfaces.
        #[serde(flatten)]
        pub extra: Extra,
    }

    /// The mesh state projected from the mesh manager.
    ///
    /// The neighbor/route/gateway lists stay as open [`Value`]s because their
    /// entry shapes evolve with the mesh carrier; modeling them here would
    /// freeze a contract the producer still moves.
    #[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
    pub struct MeshState {
        /// Directly heard mesh neighbors.
        #[serde(default)]
        pub neighbors: Vec<Value>,
        /// Known mesh routes.
        #[serde(default)]
        pub routes: Vec<Value>,
        /// Elected/known cloud gateways.
        #[serde(default)]
        pub gateways: Vec<Value>,
        /// This node's mesh role (e.g. `direct`, `relay`, `receiver`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub role: Option<String>,
        /// Any extra mesh fields.
        #[serde(flatten)]
        pub extra: Extra,
    }

    /// One systemd-managed agent service as the service-list route reports it.
    #[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
    pub struct ServiceEntry {
        /// Unit name (e.g. `ados-supervisor`).
        pub name: String,
        /// High-level state (e.g. `active`, `failed`, `inactive`).
        pub state: String,
        /// Whether the unit is currently active.
        #[serde(default)]
        pub active: bool,
        /// systemd sub-state (e.g. `running`, `exited`, `dead`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub sub_state: Option<String>,
        /// Resident memory in megabytes, when the unit reports it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub memory_mb: Option<f64>,
        /// Any extra per-service fields.
        #[serde(flatten)]
        pub extra: Extra,
    }

    /// The agent service list.
    #[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
    pub struct ServicesResponse {
        /// The services, in the order the route returns them.
        #[serde(default)]
        pub services: Vec<ServiceEntry>,
        /// Any extra envelope fields.
        #[serde(flatten)]
        pub extra: Extra,
    }

    /// Whether this agent is enrolled in a fleet.
    #[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
    pub struct FleetEnrollment {
        /// Enrollment flag.
        #[serde(default)]
        pub enrolled: bool,
        /// Any extra enrollment fields (fleet id, enrolled-at, owner).
        #[serde(flatten)]
        pub extra: Extra,
    }

    /// The fleet peer list. Peer entries stay open because the peer shape is
    /// carried by the fleet layer, not frozen here.
    #[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
    pub struct FleetPeers {
        /// The known fleet peers.
        #[serde(default)]
        pub peers: Vec<Value>,
        /// Any extra envelope fields.
        #[serde(flatten)]
        pub extra: Extra,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Map};
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Spawn a one-shot HTTP server that returns `body` with `status`, and
    /// return the base URL pointing at it.
    fn one_shot_server(status_line: &'static str, body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf); // consume the request line + headers
                let response = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn get_json_parses_a_200_response() {
        let base = one_shot_server("200 OK", r#"{"armed":false,"mode":"STABILIZE"}"#);
        let client = RestClient::new(base);
        let v = client.status().unwrap();
        assert_eq!(v["mode"], "STABILIZE");
        assert_eq!(v["armed"], false);
    }

    #[test]
    fn non_2xx_maps_to_status_error() {
        let base = one_shot_server("401 Unauthorized", r#"{"detail":"missing key"}"#);
        let client = RestClient::new(base).with_api_key("wrong");
        match client.status() {
            Err(RestError::Status(401)) => {}
            other => panic!("expected 401 status error, got {other:?}"),
        }
    }

    #[test]
    fn transport_error_when_nothing_listens() {
        // Port 1 is privileged and nothing listens; connect fails fast.
        let client = RestClient::new("http://127.0.0.1:1");
        assert!(matches!(client.status(), Err(RestError::Transport(_))));
    }

    /// Round-trip a value through JSON and assert it is unchanged. Run twice
    /// over: as the typed struct (Rust equality) and as the re-parsed JSON
    /// (wire equality), so a dropped or reordered field shows up either way.
    fn assert_round_trip<T>(value: &T)
    where
        T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let wire = serde_json::to_value(value).unwrap();
        let back: T = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(value, &back, "typed value changed across a round-trip");
        let rewire = serde_json::to_value(&back).unwrap();
        assert_eq!(wire, rewire, "wire JSON changed across a round-trip");
    }

    #[test]
    fn param_entry_round_trips_with_renamed_type_field() {
        let entry = ParamEntry {
            name: "WPNAV_SPEED".into(),
            value: 500.0,
            ptype: Some("REAL32".into()),
            extra: Map::new(),
        };
        assert_round_trip(&entry);
        // The `ptype` field serializes as the wire key `type`.
        let wire = serde_json::to_value(&entry).unwrap();
        assert_eq!(wire["type"], "REAL32");
        assert!(wire.get("ptype").is_none());
    }

    #[test]
    fn params_response_round_trips() {
        let resp = ParamsResponse {
            params: vec![
                ParamEntry {
                    name: "ARMING_CHECK".into(),
                    value: 1.0,
                    ptype: Some("INT32".into()),
                    extra: Map::new(),
                },
                ParamEntry {
                    name: "BATT_CAPACITY".into(),
                    value: 5200.0,
                    ptype: None,
                    extra: Map::new(),
                },
            ],
            extra: Map::new(),
        };
        assert_round_trip(&resp);
    }

    #[test]
    fn wfb_link_state_round_trips() {
        let state = WfbLinkState {
            status: "locked".into(),
            active: true,
            channel: Some(149),
            rssi_dbm: Some(-51),
            valid_rx_packets_per_s: Some(598.0),
            tx_bytes_per_s: Some(750_000.0),
            extra: Map::new(),
        };
        assert_round_trip(&state);
    }

    #[test]
    fn mesh_state_round_trips() {
        let state = MeshState {
            neighbors: vec![json!({"mac": "aa:bb:cc", "rssi": -60})],
            routes: vec![json!({"dest": "bat0", "metric": 2})],
            gateways: vec![],
            role: Some("relay".into()),
            extra: Map::new(),
        };
        assert_round_trip(&state);
    }

    #[test]
    fn services_response_round_trips() {
        let resp = ServicesResponse {
            services: vec![ServiceEntry {
                name: "ados-supervisor".into(),
                state: "active".into(),
                active: true,
                sub_state: Some("running".into()),
                memory_mb: Some(24.5),
                extra: Map::new(),
            }],
            extra: Map::new(),
        };
        assert_round_trip(&resp);
    }

    #[test]
    fn fleet_shapes_round_trip() {
        assert_round_trip(&FleetEnrollment {
            enrolled: true,
            extra: Map::new(),
        });
        assert_round_trip(&FleetPeers {
            peers: vec![json!({"id": "local-abc", "online": true})],
            extra: Map::new(),
        });
    }

    #[test]
    fn unknown_fields_survive_the_flatten_map() {
        // A producer one version ahead attaches a field the struct does not
        // model. It must be captured in `extra` and re-emitted unchanged.
        let raw = json!({
            "status": "locked",
            "active": true,
            "channel": 149,
            "antenna_diversity": {"best": 1, "count": 4},
            "fec_recovered": 12
        });
        let state: WfbLinkState = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(state.extra["fec_recovered"], 12);
        assert_eq!(state.extra["antenna_diversity"]["best"], 1);
        // The captured fields ride back out on serialization.
        let back = serde_json::to_value(&state).unwrap();
        assert_eq!(back, raw);
    }

    #[test]
    fn missing_optional_fields_default_cleanly() {
        // Only the always-present summary fields are sent; the numeric link
        // stats are absent. Deserialization must not fail, and the absent
        // optionals must not be re-emitted (skip_serializing_if).
        let raw = json!({"status": "down", "active": false});
        let state: WfbLinkState = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(state.channel, None);
        assert_eq!(state.rssi_dbm, None);
        let back = serde_json::to_value(&state).unwrap();
        assert_eq!(back, raw);
    }
}
