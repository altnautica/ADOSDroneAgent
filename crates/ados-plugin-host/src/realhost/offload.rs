//! Plugin perception-offload sessions: bounds, the supervised orchestrator
//! lane, and the node reach helpers.

use super::*;

/// A live streaming perception-offload session a plugin opened via
/// `compute.stream.open`. The host holds the cancel handle (to close the lane on
/// an explicit `compute.stream.close`, or when the plugin disconnects) plus the
/// node reach (to read the session's live health from `/api/compute/sessions`).
pub(super) struct OffloadStreamHandle {
    /// The plugin that opened it, so a disconnect closes only its own streams.
    pub(super) plugin_id: String,
    /// Cancels the orchestrator task — the graceful close that settles the
    /// offload safety gate to Lost before the lane ends.
    pub(super) cancel: Arc<Notify>,
    /// The orchestrator task; aborted on close as a belt-and-braces reap (the
    /// cancel is the graceful path).
    pub(super) task: JoinHandle<()>,
    /// The compute node's job-API base URL the session runs against (for health).
    pub(super) node_base_url: String,
    /// The credential the node issued this drone (`None` when none is installed).
    pub(super) credential: Option<String>,
}

/// The RTSP port the drone's encoder publishes its primary feed on, and the
/// stream path — the node pulls `rtsp://<drone-lan-ip>:8554/main` (the same feed
/// the auto-offload reconciler streams). The node ingests this source.
pub(super) const OFFLOAD_RTSP_PORT: u16 = 8554;
pub(super) const OFFLOAD_RTSP_PATH: &str = "main";
/// Default decoded frame size for an offload session when the plugin omits it
/// (the orchestrator's own default). The plugin passes its camera size so the
/// node's RGB24 decode matches; this is the safety default.
pub(super) const OFFLOAD_DEFAULT_WIDTH: u32 = 1280;
pub(super) const OFFLOAD_DEFAULT_HEIGHT: u32 = 720;
/// Bounds on a plugin-supplied frame dimension. The node allocates an RGB24
/// buffer of width x height per frame, so an unbounded value is a memory lever
/// on the node.
pub(super) const OFFLOAD_MIN_DIMENSION: i64 = 16;
pub(super) const OFFLOAD_MAX_DIMENSION: i64 = 4096;
/// Default freshness budget (ms) for the returned detection stream: past this
/// with no new batch, the offload safety gate trips the designated lock to Lost.
pub(super) const OFFLOAD_DEFAULT_BUDGET_MS: i64 = 700;
/// Bounds on a plugin-supplied freshness budget. The budget is a safety limit
/// the host enforces on the plugin's behalf: a budget of hours would keep a
/// designated lock alive on a stalled link.
pub(super) const OFFLOAD_MIN_BUDGET_MS: i64 = 100;
pub(super) const OFFLOAD_MAX_BUDGET_MS: i64 = 2000;

/// The decoded frame `(width, height)` and detection freshness budget (ms) an
/// offload session runs with. An absent or non-positive value takes the
/// default; a positive one is clamped into its bounds.
pub(super) fn offload_frame_and_budget(args: &Value) -> (u32, u32, i64) {
    let dimension = |key: &str, default: u32| {
        arg_i64(args, key).filter(|n| *n > 0).map_or(default, |n| {
            n.clamp(OFFLOAD_MIN_DIMENSION, OFFLOAD_MAX_DIMENSION) as u32
        })
    };
    let budget = arg_i64(args, "target_budget_ms")
        .filter(|n| *n > 0)
        .map_or(OFFLOAD_DEFAULT_BUDGET_MS, |n| {
            n.clamp(OFFLOAD_MIN_BUDGET_MS, OFFLOAD_MAX_BUDGET_MS)
        });
    (
        dimension("width", OFFLOAD_DEFAULT_WIDTH),
        dimension("height", OFFLOAD_DEFAULT_HEIGHT),
        budget,
    )
}

/// Everything one plugin offload session needs to (re)start its orchestrator.
pub(super) struct OffloadLane {
    pub(super) session_id: String,
    pub(super) camera_id: String,
    pub(super) rtsp_url: String,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) target_budget_ms: i64,
    pub(super) model_id: Option<String>,
    pub(super) base_url: String,
    pub(super) credential: Option<String>,
}

/// Run a plugin offload session until its handle is aborted: the orchestrator
/// ends when the node restarts, the node stream closes, or a submit fails, and
/// each time it is started again after the host's fixed reconnect interval, so
/// a session the plugin still holds open keeps delivering detections once the
/// node is back.
pub(super) async fn supervise_offload_lane(lane: OffloadLane, cancel: Arc<Notify>) {
    loop {
        let mut cfg = OrchestratorConfig::vision_only(
            lane.session_id.clone(),
            lane.camera_id.clone(),
            lane.rtsp_url.clone(),
            lane.width,
            lane.height,
            lane.target_budget_ms,
        );
        if let Some(m) = &lane.model_id {
            cfg.model_id = m.clone();
        }
        let endpoint = NodeEndpoint {
            base_url: lane.base_url.clone(),
            credential: lane.credential.clone(),
        };
        match run_offload_orchestrator(cfg, endpoint, cancel.clone()).await {
            Ok(()) => {
                tracing::info!(session = %lane.session_id, "plugin offload stream ended; restarting")
            }
            Err(e) => {
                tracing::warn!(session = %lane.session_id, error = %e, "plugin offload stream failed; restarting")
            }
        }
        tokio::time::sleep(RECONNECT_INTERVAL).await;
    }
}

pub(super) fn now_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The drone's egress IP toward `node_addr` (`host:port`) — the source address a
/// connection to the node would use, i.e. the address the node can pull the
/// drone's RTSP feed back on. The name is resolved through the runtime's
/// resolver (off the worker); a UDP "connect" to the resolved address then
/// picks the source IP without sending a packet. `None` when the node address
/// does not resolve or no route exists (no verified reach, so no address is
/// advertised). Mirrors the offload reconciler's egress probe (which owns the
/// auto path); this crate has no build dependency on it, so the tiny probe is
/// duplicated (not tier logic).
pub(super) async fn local_ip_towards(node_addr: &str) -> Option<std::net::IpAddr> {
    let addr = tokio::net::lookup_host(node_addr).await.ok()?.next()?;
    let sock = std::net::UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    sock.connect(addr).ok()?;
    sock.local_addr().ok().map(|a| a.ip())
}

/// The credential to present to the offload node: the one the node named by
/// `node_id` (the offload link's advertised node id) issued this drone, or, for
/// a node with no advertised id, the sole installed credential. `None` when none
/// fits, in which case a paired node refuses the session. Never the drone's own
/// pairing key, which is full authority over the drone and means nothing to the
/// node.
pub(super) fn workstation_credential(
    path: &std::path::Path,
    node_id: Option<&str>,
) -> Option<String> {
    WorkstationCredentials::load_or_empty(path)
        .for_node(node_id)
        .map(|c| c.credential.clone())
}
