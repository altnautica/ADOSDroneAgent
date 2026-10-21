//! REST client and shared wire types for the agent HTTP API (Contract D, port
//! 8080).
//!
//! The native front (`ados-control`) serves the API on `:8080`; this is the
//! client side used by the Rust TUI (and any other Rust REST consumer). Auth is
//! the `X-ADOS-Key` header, or a dashboard PIN session in
//! `X-ADOS-Dashboard-Session`; the pairing routes are reachable while unpaired.
//! The client is blocking (`ureq`, HTTP-only) because its callers are simple
//! pollers and worker threads off the flight-critical path.
//!
//! The client returns raw [`serde_json::Value`] bodies. The [`types`] submodule
//! adds typed serde structs for the read-projection shapes that the native
//! route handlers project out of subsystem sockets and sidecars. Defining each
//! shape once here keeps the data contract in the shared protocol crate, so a
//! handler does not re-invent an ad-hoc struct per route. Every type carries an
//! open `extra` map (via `#[serde(flatten)]`) and `#[serde(default)]` fields, so
//! a new field on the producer side round-trips through a consumer that does not
//! yet know about it.

use std::io::Read;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use serde_json::Value;
use thiserror::Error;

pub use types::*;

/// Default base URL: the agent's local HTTP API. Matches `cli/main.py`'s
/// `API_BASE`.
pub const DEFAULT_BASE: &str = "http://localhost:8080";

/// Default per-request timeout, matching the Python CLI's httpx client.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(8);

/// Connect timeout for a long-lived stream (server-sent events). The stream
/// itself has no read timeout: an idle log stream is not a failure.
pub const STREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Error)]
pub enum RestError {
    /// A non-2xx answer, with its JSON body when the server sent one.
    #[error("http {code}{suffix}", suffix = status_detail_suffix(.body.as_ref()))]
    Status { code: u16, body: Option<Value> },
    #[error("transport error: {0}")]
    Transport(String),
    #[error("response was not valid json: {0}")]
    Json(String),
}

impl RestError {
    /// The HTTP status code, when the server answered.
    pub fn code(&self) -> Option<u16> {
        match self {
            RestError::Status { code, .. } => Some(*code),
            _ => None,
        }
    }

    /// The JSON body of a non-2xx answer, when there was one.
    pub fn body(&self) -> Option<&Value> {
        match self {
            RestError::Status { body, .. } => body.as_ref(),
            _ => None,
        }
    }

    /// The human detail of a non-2xx answer: the body's `detail` string, else
    /// `message`, else `error`. Empty when the body carries none of them.
    pub fn detail(&self) -> String {
        self.body().map(status_detail).unwrap_or_default()
    }
}

/// The human detail of an error body. Flat bodies carry `detail`, `message` or
/// `error` as a string; nested ones carry an object under `detail` and/or
/// `error` (`{"detail": {"error": {"code", "message"}}}`). The first string
/// found among `detail`, `message`, `error`, each looked up directly and then
/// as `<k>.message` and `<k>.error.message`, wins; empty when there is none.
pub fn status_detail(body: &Value) -> String {
    const PATHS: [&[&str]; 9] = [
        &["detail"],
        &["message"],
        &["error"],
        &["detail", "message"],
        &["detail", "error", "message"],
        &["detail", "error"],
        &["error", "message"],
        &["detail", "error", "code"],
        &["error", "code"],
    ];
    PATHS
        .iter()
        .find_map(|p| p.iter().try_fold(body, |v, k| v.get(*k)).and_then(Value::as_str))
        .unwrap_or_default()
        .to_string()
}

fn status_detail_suffix(body: Option<&Value>) -> String {
    match body.map(status_detail) {
        Some(d) if !d.is_empty() => format!(": {d}"),
        _ => String::new(),
    }
}

/// The credentials a [`RestClient`] sends. Shared by every clone of a client,
/// so setting a key or session on one applies to all of them.
#[derive(Debug, Clone, Default)]
pub struct Auth {
    /// Pairing key, sent as `X-ADOS-Key`.
    pub api_key: Option<String>,
    /// Dashboard PIN session, sent as `X-ADOS-Dashboard-Session`.
    pub session: Option<String>,
}

/// A blocking REST client for the agent HTTP API. Cheap to clone; clones share
/// one [`Auth`].
#[derive(Clone)]
pub struct RestClient {
    base: String,
    auth: Arc<RwLock<Auth>>,
    agent: ureq::Agent,
    stream_agent: ureq::Agent,
}

impl RestClient {
    /// Build a client for the given base URL (no trailing slash).
    pub fn new(base: impl Into<String>) -> Self {
        let agent = ureq::AgentBuilder::new().timeout(DEFAULT_TIMEOUT).build();
        let stream_agent = ureq::AgentBuilder::new()
            .timeout_connect(STREAM_CONNECT_TIMEOUT)
            .build();
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            auth: Arc::default(),
            agent,
            stream_agent,
        }
    }

    /// Client for the default local agent (`http://localhost:8080`).
    pub fn local() -> Self {
        Self::new(DEFAULT_BASE)
    }

    /// Attach the pairing key sent as `X-ADOS-Key` on every request.
    pub fn with_api_key(self, key: impl Into<String>) -> Self {
        self.set_api_key(Some(key.into()));
        self
    }

    /// Set or clear the pairing key on this client and every clone of it.
    pub fn set_api_key(&self, key: Option<String>) {
        self.auth
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .api_key = key;
    }

    /// Set or clear the dashboard PIN session on this client and every clone.
    pub fn set_session(&self, session: Option<String>) {
        self.auth
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .session = session;
    }

    /// The credentials currently attached.
    pub fn auth(&self) -> Auth {
        self.auth
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The base URL, e.g. `http://localhost:8080`.
    pub fn base(&self) -> &str {
        &self.base
    }

    fn request(&self, agent: &ureq::Agent, method: &str, path: &str) -> ureq::Request {
        let url = format!("{}{}", self.base, path);
        let mut req = agent.request(method, &url);
        let auth = self.auth();
        if let Some(key) = auth.api_key.as_deref() {
            req = req.set("X-ADOS-Key", key);
        }
        if let Some(session) = auth.session.as_deref() {
            req = req.set("X-ADOS-Dashboard-Session", session);
        }
        req
    }

    /// GET `path` (e.g. `/api/status`) and parse the JSON body.
    pub fn get_json(&self, path: &str) -> Result<Value, RestError> {
        self.send_json("GET", path, None, DEFAULT_TIMEOUT)
    }

    /// GET `path` with its own request timeout (a slow route such as a Wi-Fi
    /// scan).
    pub fn get_json_timeout(&self, path: &str, timeout: Duration) -> Result<Value, RestError> {
        self.send_json("GET", path, None, timeout)
    }

    /// Send `method path` with an optional JSON body and parse the JSON reply.
    /// An empty 2xx body is [`Value::Null`].
    pub fn send_json(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        timeout: Duration,
    ) -> Result<Value, RestError> {
        let req = self.request(&self.agent, method, path).timeout(timeout);
        let result = match body {
            Some(b) => req.send_json(b),
            None => req.call(),
        };
        match result {
            Ok(resp) => {
                let text = resp
                    .into_string()
                    .map_err(|e| RestError::Json(e.to_string()))?;
                if text.trim().is_empty() {
                    return Ok(Value::Null);
                }
                serde_json::from_str(&text).map_err(|e| RestError::Json(e.to_string()))
            }
            Err(ureq::Error::Status(code, resp)) => Err(RestError::Status {
                code,
                body: resp.into_json::<Value>().ok(),
            }),
            Err(ureq::Error::Transport(t)) => Err(RestError::Transport(t.to_string())),
        }
    }

    /// Open a server-sent-events stream on `path`. Only the connect is
    /// time-bounded; the returned reader blocks until the server writes or
    /// closes.
    pub fn open_stream(&self, path: &str) -> Result<Box<dyn Read + Send>, RestError> {
        let req = self
            .request(&self.stream_agent, "GET", path)
            .set("Accept", "text/event-stream");
        match req.call() {
            Ok(resp) => Ok(Box::new(resp.into_reader())),
            Err(ureq::Error::Status(code, resp)) => Err(RestError::Status {
                code,
                body: resp.into_json::<Value>().ok(),
            }),
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

    /// Progress of the parameter-cache priming sweep.
    #[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
    pub struct ParamProgress {
        /// Parameters cached so far.
        #[serde(default)]
        pub got: u64,
        /// The FC-advertised total (0 while unknown).
        #[serde(default)]
        pub expected: u64,
    }

    /// The flight-controller parameter list (`GET /api/params`).
    #[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
    pub struct ParamsResponse {
        /// Parameter name to value. Parameters are carried as floats on the
        /// wire; an integer parameter is the exact float of that integer.
        #[serde(default)]
        pub params: std::collections::BTreeMap<String, f64>,
        /// The FC-advertised total when known, else the cached count.
        #[serde(default)]
        pub count: u64,
        /// How many parameters have landed in the cache.
        #[serde(default)]
        pub cached: u64,
        /// Whether the priming sweep is still running.
        #[serde(default)]
        pub priming: bool,
        /// The FC stayed silent past the sweep deadline.
        #[serde(default)]
        pub priming_timeout: bool,
        /// The sweep request itself failed at the link layer.
        #[serde(default)]
        pub priming_send_failed: bool,
        /// Sweep progress, when reported.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub progress: Option<ParamProgress>,
        /// Any extra envelope fields.
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
            Err(e @ RestError::Status { code: 401, .. }) => {
                assert_eq!(e.detail(), "missing key");
                assert_eq!(e.to_string(), "http 401: missing key");
            }
            other => panic!("expected 401 status error, got {other:?}"),
        }
    }

    #[test]
    fn nested_error_bodies_yield_their_message() {
        let nested = json!({"detail": {"error": {"code": "E_WIFI_STATUS_UNAVAILABLE", "message": "nmcli missing"}}});
        assert_eq!(status_detail(&nested), "nmcli missing");
        let coded = json!({"detail": {"error": {"code": "E_RIG_INVALID"}}});
        assert_eq!(status_detail(&coded), "E_RIG_INVALID");
        assert_eq!(status_detail(&json!({"detail": {"message": "m"}})), "m");
        assert_eq!(status_detail(&json!({"other": 1})), "");
    }

    #[test]
    fn an_error_body_rides_the_status_error() {
        let base = one_shot_server(
            "409 Conflict",
            r#"{"error":"E_ARMED","message":"vehicle is armed"}"#,
        );
        let client = RestClient::new(base);
        let err = client
            .send_json("PUT", "/api/config", Some(&json!({"key": "a", "value": 1})), DEFAULT_TIMEOUT)
            .unwrap_err();
        assert_eq!(err.code(), Some(409));
        assert_eq!(err.body().unwrap()["error"], "E_ARMED");
        // `message` outranks `error` as the human detail.
        assert_eq!(err.detail(), "vehicle is armed");
    }

    #[test]
    fn an_empty_2xx_body_is_null() {
        let base = one_shot_server("204 No Content", "");
        let client = RestClient::new(base);
        let v = client
            .send_json("DELETE", "/api/v1/network/client", None, DEFAULT_TIMEOUT)
            .unwrap();
        assert_eq!(v, Value::Null);
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
    fn params_response_reads_the_wire_map() {
        let raw = json!({
            "params": {"ARMING_CHECK": 1.0, "BATT_CAPACITY": 5200.0},
            "count": 700,
            "cached": 2,
            "priming": true,
            "priming_timeout": false,
            "priming_send_failed": false,
            "progress": {"got": 2, "expected": 700}
        });
        let resp: ParamsResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.params["BATT_CAPACITY"], 5200.0);
        assert_eq!(resp.progress, Some(ParamProgress { got: 2, expected: 700 }));
        assert!(resp.priming);
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
