//! Auth for the compute node's listener: the owner's job API and the lanes
//! other nodes use.
//!
//! The node mirrors the agent's data-plane posture (the shared
//! [`ados_protocol::pairing_posture`] primitives the HTTP control surface and the
//! MAVLink WS proxy already use):
//!
//! - **Unpaired ⇒ open.** A fresh node has no key; LAN presence is the gate, the
//!   same stance the pairing flow takes, so a GCS can discover and claim it.
//! - **Paired + on-box ⇒ open.** A loopback caller that was not relayed by a
//!   proxy already holds shell-level privilege that exceeds API auth.
//! - **Paired + off-box ⇒ a credential.** The owner presents the pairing key in
//!   `X-ADOS-Key` (compared in constant time) and reaches everything. Another
//!   node presents the credential this node issued it
//!   ([`crate::node_credentials`]) in the node-credential header, and reaches
//!   only the lanes it was issued for. A browser, which cannot set a header on a
//!   WebSocket handshake, reaches the world-model stream with a short-lived
//!   ticket the owner minted, offered as a subprotocol.
//!
//! [`require_job_api`] guards the job API: owner-only except the two calls a
//! drone's offload session makes ([`job_api_lane`]). [`require_lane`] guards
//! each lane router. There is no public exempt set: an unpaired node is already
//! fully open, and a paired node leaks nothing to an unauthenticated LAN caller.
//! With these gates in place the daemon may bind a non-loopback address safely.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;

use ados_protocol::node_credential::{NodeLane, NODE_CREDENTIAL_HEADER};
use ados_protocol::pairing_posture::{
    classify_caller, data_plane_access, load_pairing, Access, CallerClass, Pairing,
};
use ados_protocol::ws_ticket::{
    now_unix, WsTicketIssuer, SCOPE_ATLAS_WORLD_WS, WS_TICKET_SUBPROTOCOL,
};

use crate::node_credentials::NodeCredentialStore;

/// Default pairing-state path: the agent's `pairing.json`.
pub const DEFAULT_PAIRING_PATH: &str = "/etc/ados/pairing.json";

/// The header the owner presents the pairing key in.
const KEY_HEADER: &str = "x-ados-key";

/// How long a loaded pairing state is trusted before the file is re-read. Short
/// enough that a pair/unpair is honoured within a couple of requests, long enough
/// that a request burst does not stat the file every time.
const PAIRING_TTL: Duration = Duration::from_secs(2);

/// Reads `pairing.json` and answers the auth question, with a short-TTL cache so
/// the file is not stat-ed on every request. Held behind an `Arc` and shared by
/// the middleware.
pub struct PairingGate {
    path: PathBuf,
    cache: Mutex<Cache>,
}

struct Cache {
    loaded: Pairing,
    at: Instant,
    primed: bool,
}

impl PairingGate {
    /// Build a gate against the agent's standard pairing path.
    pub fn new() -> Self {
        Self::with_path(PathBuf::from(DEFAULT_PAIRING_PATH))
    }

    /// Build a gate against an explicit path (the daemon's env override, tests).
    pub fn with_path(path: PathBuf) -> Self {
        Self {
            path,
            cache: Mutex::new(Cache {
                loaded: Pairing::Unpaired,
                at: Instant::now(),
                primed: false,
            }),
        }
    }

    /// The current pairing posture, reading the file at most once per TTL.
    pub fn current(&self) -> Pairing {
        let mut cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        if cache.primed && cache.at.elapsed() < PAIRING_TTL {
            return cache.loaded.clone();
        }
        let fresh = load_pairing(&self.path);
        cache.loaded = fresh.clone();
        cache.at = Instant::now();
        cache.primed = true;
        fresh
    }
}

impl Default for PairingGate {
    fn default() -> Self {
        Self::new()
    }
}

/// A fixed-window token bucket that guards the off-box (LAN) edge so a runaway
/// caller cannot pin the node. On-box callers are never limited (they already
/// hold shell-level privilege); one shared bucket guards the whole edge, mirroring
/// the agent's HTTP control surface.
pub struct RateLimiter {
    capacity: u32,
    window: Duration,
    state: Mutex<RateState>,
}

struct RateState {
    tokens: u32,
    window_start: Instant,
}

impl RateLimiter {
    /// A limiter granting `capacity` requests per `window`.
    pub fn new(capacity: u32, window: Duration) -> Self {
        Self {
            capacity,
            window,
            state: Mutex::new(RateState {
                tokens: capacity,
                window_start: Instant::now(),
            }),
        }
    }

    /// The default off-box budget: a generous per-second rate that status polling
    /// and job bursts both fit under.
    pub fn default_control() -> Self {
        Self::new(120, Duration::from_secs(1))
    }

    /// Try to admit one request; `false` when the window's budget is exhausted.
    pub fn check(&self) -> bool {
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if s.window_start.elapsed() >= self.window {
            s.window_start = Instant::now();
            s.tokens = self.capacity;
        }
        if s.tokens > 0 {
            s.tokens -= 1;
            true
        } else {
            false
        }
    }
}

/// The auth state the middleware carries: the pairing gate, the off-box rate
/// limiter for the job API, and the credentials this node issued.
pub struct ComputeAuth {
    pub gate: PairingGate,
    pub limiter: RateLimiter,
    pub credentials: NodeCredentialStore,
}

impl ComputeAuth {
    /// Build the auth state against a pairing-state path and the issued
    /// credential store, with the default off-box rate budget.
    pub fn new(pairing_path: PathBuf, credentials: NodeCredentialStore) -> Self {
        Self {
            gate: PairingGate::with_path(pairing_path),
            limiter: RateLimiter::default_control(),
            credentials,
        }
    }
}

/// The node lane a job-API request belongs to, when another node may make it:
/// submitting an offload session and reading session health. Everything else
/// on the job API (listing, cancelling, datasets, outputs, issuing credentials)
/// is the owner's alone.
pub fn job_api_lane(method: &Method, path: &str) -> Option<NodeLane> {
    match (method, path) {
        (&Method::POST, "/api/compute/jobs") | (&Method::GET, "/api/compute/sessions") => {
            Some(NodeLane::JobSubmit)
        }
        _ => None,
    }
}

/// The credentials a request presented.
#[derive(Debug, Default, Clone, Copy)]
pub struct Presented<'a> {
    /// The owner's pairing key (`X-ADOS-Key`).
    pub owner_key: Option<&'a str>,
    /// A node credential this node issued.
    pub node_credential: Option<&'a str>,
    /// A world-stream ticket offered as a WebSocket subprotocol.
    pub ws_ticket: Option<&'a str>,
}

/// Decide one request. `lane` is the node lane the route belongs to, or `None`
/// for an owner-only route.
pub fn decide(
    pairing: &Pairing,
    caller: CallerClass,
    presented: Presented<'_>,
    lane: Option<NodeLane>,
    credentials: &NodeCredentialStore,
    now_unix: i64,
) -> Access {
    if data_plane_access(pairing, caller, presented.owner_key) == Access::Accept {
        return Access::Accept;
    }
    let (Some(lane), Pairing::Paired(owner_key)) = (lane, pairing) else {
        return Access::Unauthorized;
    };
    if presented
        .node_credential
        .is_some_and(|c| credentials.admits(c, lane, owner_key))
    {
        return Access::Accept;
    }
    if lane == NodeLane::AtlasWorld
        && presented.ws_ticket.is_some_and(|t| {
            WsTicketIssuer::from_api_key(owner_key)
                .verify(t, SCOPE_ATLAS_WORLD_WS, now_unix)
                .is_ok()
        })
    {
        return Access::Accept;
    }
    Access::Unauthorized
}

/// The ticket following the [`WS_TICKET_SUBPROTOCOL`] marker in the offered
/// `Sec-WebSocket-Protocol` list, if any. The ticket is pipe-delimited and
/// carries no comma, so it survives the comma split intact.
fn offered_ticket(headers: &HeaderMap) -> Option<String> {
    let offered: Vec<String> = headers
        .get_all("sec-websocket-protocol")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    let pos = offered.iter().position(|p| p == WS_TICKET_SUBPROTOCOL)?;
    offered.get(pos + 1).cloned()
}

/// Classify the caller from the peer [`ConnectInfo`] (present when served with
/// `into_make_service_with_connect_info`). With no peer (a `oneshot` test that
/// injects none) the caller reads off-box, the conservative default. A proxy or
/// tunnel that SETS a forwarding header
/// ([`ados_protocol::pairing_posture::FORWARDED_HEADERS`]) is denied loopback
/// trust so it cannot impersonate an on-box caller; a raw L4 tunnel that sets
/// none is, like a co-resident process, on-box by the same trust model.
fn caller_of(req: &Request) -> CallerClass {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip());
    // axum's connect info carries the peer only; the route toward the peer
    // names the local address it reached.
    let local = peer.and_then(ados_protocol::pairing_posture::local_addr_toward);
    classify_caller(peer, local, |h| req.headers().contains_key(h))
}

fn access_for(
    auth: &ComputeAuth,
    req: &Request,
    caller: CallerClass,
    lane: Option<NodeLane>,
) -> Access {
    let headers = req.headers();
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let ticket = match lane {
        Some(NodeLane::AtlasWorld) => offered_ticket(headers),
        _ => None,
    };
    decide(
        &auth.gate.current(),
        caller,
        Presented {
            owner_key: header(KEY_HEADER),
            node_credential: header(NODE_CREDENTIAL_HEADER),
            ws_ticket: ticket.as_deref(),
        },
        lane,
        &auth.credentials,
        now_unix(),
    )
}

/// A terse, state-independent 401: the status itself is the only signal.
fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({ "error": "unauthorized" })),
    )
        .into_response()
}

/// Axum middleware for the job API: the owner reaches every route; another
/// node reaches only the routes [`job_api_lane`] names, with a credential issued
/// for that lane. Off-box callers share one rate budget (429 when spent).
pub async fn require_job_api(
    State(auth): State<Arc<ComputeAuth>>,
    req: Request,
    next: Next,
) -> Response {
    let caller = caller_of(&req);
    // Rate-limit the off-box edge only; on-box callers are trusted + unlimited.
    if caller != CallerClass::OnBox && !auth.limiter.check() {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({ "error": "rate limited" })),
        )
            .into_response();
    }
    let lane = job_api_lane(req.method(), req.uri().path());
    match access_for(&auth, &req, caller, lane) {
        Access::Accept => next.run(req).await,
        Access::Unauthorized => unauthorized(),
    }
}

/// Axum middleware for one lane router: the owner, or a node credential issued
/// for `lane` (or, on the world stream, an owner-minted ticket).
pub async fn require_lane(
    State((auth, lane)): State<(Arc<ComputeAuth>, NodeLane)>,
    req: Request,
    next: Next,
) -> Response {
    let caller = caller_of(&req);
    match access_for(&auth, &req, caller, Some(lane)) {
        Access::Accept => next.run(req).await,
        Access::Unauthorized => unauthorized(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn paired_gate(dir: &std::path::Path) -> Arc<PairingGate> {
        let path = dir.join("pairing.json");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(br#"{"paired": true, "api_key": "ados_secret"}"#)
            .unwrap();
        Arc::new(PairingGate::with_path(path))
    }

    #[test]
    fn unpaired_gate_reads_open() {
        let dir = tempfile::tempdir().unwrap();
        let gate = PairingGate::with_path(dir.path().join("absent.json"));
        assert_eq!(gate.current(), Pairing::Unpaired);
    }

    #[test]
    fn paired_gate_reads_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let gate = paired_gate(dir.path());
        assert_eq!(gate.current(), Pairing::Paired("ados_secret".into()));
    }

    const OWNER: &str = "ados_secret";

    fn paired() -> Pairing {
        Pairing::Paired(OWNER.into())
    }

    fn store(dir: &std::path::Path) -> NodeCredentialStore {
        NodeCredentialStore::open(dir.join("creds.json"), "ws-node")
    }

    #[test]
    fn a_drone_credential_reaches_only_its_lanes_and_never_owner_routes() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let m = s
            .mint(
                "drone-1",
                &[NodeLane::AtlasIngest, NodeLane::JobSubmit],
                OWNER,
                1,
            )
            .unwrap();
        let with_cred = Presented {
            node_credential: Some(&m.credential),
            ..Presented::default()
        };
        let lan = CallerClass::OperatorLan;
        let now = now_unix();
        assert_eq!(
            decide(
                &paired(),
                lan,
                with_cred,
                Some(NodeLane::AtlasIngest),
                &s,
                now
            ),
            Access::Accept
        );
        assert_eq!(
            decide(
                &paired(),
                lan,
                with_cred,
                Some(NodeLane::JobSubmit),
                &s,
                now
            ),
            Access::Accept
        );
        // A lane it was not issued for.
        assert_eq!(
            decide(
                &paired(),
                lan,
                with_cred,
                Some(NodeLane::Artifacts),
                &s,
                now
            ),
            Access::Unauthorized
        );
        // An owner-only route (issuing credentials, listing jobs, ...).
        assert_eq!(
            decide(&paired(), lan, with_cred, None, &s, now),
            Access::Unauthorized
        );
        // Presented as if it were the owner key, it is nothing.
        let as_key = Presented {
            owner_key: Some(&m.credential),
            ..Presented::default()
        };
        assert_eq!(
            decide(&paired(), lan, as_key, Some(NodeLane::AtlasIngest), &s, now),
            Access::Unauthorized
        );
    }

    #[test]
    fn a_paired_lane_refuses_an_offbox_caller_with_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        for lane in NodeLane::ALL {
            assert_eq!(
                decide(
                    &paired(),
                    CallerClass::OperatorLan,
                    Presented::default(),
                    Some(lane),
                    &s,
                    now_unix()
                ),
                Access::Unauthorized,
                "{lane:?}"
            );
            // The owner key reaches every lane.
            let owner = Presented {
                owner_key: Some(OWNER),
                ..Presented::default()
            };
            assert_eq!(
                decide(
                    &paired(),
                    CallerClass::OperatorLan,
                    owner,
                    Some(lane),
                    &s,
                    now_unix()
                ),
                Access::Accept
            );
            // Unpaired and on-box stay open, per the posture.
            assert_eq!(
                decide(
                    &Pairing::Unpaired,
                    CallerClass::OperatorLan,
                    Presented::default(),
                    Some(lane),
                    &s,
                    now_unix()
                ),
                Access::Accept
            );
            assert_eq!(
                decide(
                    &paired(),
                    CallerClass::OnBox,
                    Presented::default(),
                    Some(lane),
                    &s,
                    now_unix()
                ),
                Access::Accept
            );
        }
    }

    #[test]
    fn a_world_ticket_reaches_the_world_stream_only() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let now = now_unix();
        let ticket = WsTicketIssuer::from_api_key(OWNER)
            .mint_at(SCOPE_ATLAS_WORLD_WS, 30, now)
            .token;
        let offered = Presented {
            ws_ticket: Some(&ticket),
            ..Presented::default()
        };
        assert_eq!(
            decide(
                &paired(),
                CallerClass::OperatorLan,
                offered,
                Some(NodeLane::AtlasWorld),
                &s,
                now
            ),
            Access::Accept
        );
        assert_eq!(
            decide(
                &paired(),
                CallerClass::OperatorLan,
                offered,
                Some(NodeLane::Artifacts),
                &s,
                now
            ),
            Access::Unauthorized
        );
        // Expired, or signed by another key.
        assert_eq!(
            decide(
                &paired(),
                CallerClass::OperatorLan,
                offered,
                Some(NodeLane::AtlasWorld),
                &s,
                now + 31
            ),
            Access::Unauthorized
        );
        let forged = WsTicketIssuer::from_api_key("other")
            .mint_at(SCOPE_ATLAS_WORLD_WS, 30, now)
            .token;
        let forged = Presented {
            ws_ticket: Some(&forged),
            ..Presented::default()
        };
        assert_eq!(
            decide(
                &paired(),
                CallerClass::OperatorLan,
                forged,
                Some(NodeLane::AtlasWorld),
                &s,
                now
            ),
            Access::Unauthorized
        );
    }

    #[test]
    fn only_offload_submit_and_session_health_are_node_reachable_on_the_job_api() {
        assert_eq!(
            job_api_lane(&Method::POST, "/api/compute/jobs"),
            Some(NodeLane::JobSubmit)
        );
        assert_eq!(
            job_api_lane(&Method::GET, "/api/compute/sessions"),
            Some(NodeLane::JobSubmit)
        );
        for (m, p) in [
            (Method::GET, "/api/compute/jobs"),
            (Method::GET, "/api/compute/status"),
            (Method::POST, "/api/compute/jobs/j/cancel"),
            (Method::POST, "/api/compute/datasets"),
            (Method::POST, "/api/compute/node-credentials"),
            (Method::POST, "/api/compute/ws-ticket"),
        ] {
            assert_eq!(job_api_lane(&m, p), None, "{m} {p}");
        }
    }

    #[test]
    fn the_ticket_is_read_after_its_marker() {
        let mut h = HeaderMap::new();
        h.insert(
            "sec-websocket-protocol",
            "ados-ws-ticket, v1|s|1|2|ff".parse().unwrap(),
        );
        assert_eq!(offered_ticket(&h).as_deref(), Some("v1|s|1|2|ff"));
        h.insert("sec-websocket-protocol", "ados-ws-ticket".parse().unwrap());
        assert_eq!(offered_ticket(&h), None);
    }
}
