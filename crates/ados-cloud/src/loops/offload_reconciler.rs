//! The drone-side perception offload reconciler.
//!
//! An NPU-less drone runs its detection on a paired workstation instead of on
//! board. This loop makes that automatic: each tick, when the board has no
//! accelerator (or the operator forced offload on) and a `profile=workstation`
//! node is reachable on the LAN and the camera is up, it starts (and supervises)
//! the offload orchestrator — the drone streams its RTSP camera to the node, the
//! node runs the detector, and detections return onto the drone's own
//! `vision.detection` bus (transparent to every consumer). It writes the
//! `/run/ados/offload-link.json` sidecar so the status surfaces report (and
//! surface the target of) the live offload; absent/idle ⇒ the drone reports
//! `none` (Rule 44 — never a fabricated paired node).
//!
//! Local-first (Rule 39): the node is discovered over mDNS (or pinned by config),
//! reached by its LAN job-API address; no cloud round-trip. The RTSP URL handed
//! to the node is the drone's LAN-reachable egress IP (never `localhost` — the
//! node pulls the feed).
//!
//! INERT by default off a drone: it early-returns on a non-drone profile or when
//! `perception.offload.enabled = off`, so a workstation / ground station / an
//! opted-out drone does no offload work and is byte-unchanged. The default
//! `auto` runs it on every drone (the automatic path).

use std::net::{IpAddr, ToSocketAddrs, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ados_compute::{resolve_compute, run_offload_orchestrator, NodeEndpoint, OrchestratorConfig};
use ados_protocol::offload_link::{write_offload_link, OffloadLink};
use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;

use crate::config::{perception_offload_addr, CloudConfig};
use crate::pairing::PairingState;

/// The reconcile cadence. Comfortably under the sidecar's 20 s staleness window
/// so an active link's mtime stays fresh, and responsive enough to pick up a
/// workstation shortly after it appears.
const TICK: Duration = Duration::from_secs(5);
/// One mDNS browse's timeout.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(3);
/// While searching (no active session), don't re-browse mDNS more often than
/// this — a browse every tick is wasteful when no node is present.
const SEARCH_BACKOFF: Duration = Duration::from_secs(15);
/// The mediamtx RTSP port + main path the drone's encoder publishes to.
const RTSP_PORT: u16 = 8554;
const RTSP_PATH: &str = "main";
/// The compute node's default job-API port (used when a pinned addr omits one).
const DEFAULT_JOB_API_PORT: u16 = 8092;
/// The camera the offloaded detections are attributed to (the drone's primary).
const CAMERA_ID: &str = "front";
/// The freshness budget (ms) for the returned detection stream: past this with no
/// new batch, the `ados-offload` gate trips the lock to Lost (stale = lost).
const TARGET_BUDGET_MS: i64 = 700;
/// The board fingerprint sidecar (carries `npu_tops`).
const BOARD_JSON: &str = "/run/ados/board.json";
/// The camera pipeline readiness sidecar.
const CAMERA_STATE_JSON: &str = "/run/ados/camera-state.json";

/// The drone's live offload-session state the reconciler owns.
struct RunningSession {
    /// Cancels the orchestrator task.
    cancel: Arc<Notify>,
    /// The orchestrator task (finishes when the node/stream drops).
    handle: JoinHandle<()>,
    /// The node address this session offloads to (`host:port`).
    target: String,
}

/// A per-tick decision — resolved (blocking / async) into concrete offload facts,
/// or idle (do not offload).
enum Decision {
    Offload {
        base_url: String,
        target: String,
        node_device_id: Option<String>,
        rtsp_url: String,
        width: u32,
        height: u32,
    },
    Idle,
}

/// Whether an offload attempt is even warranted from the config + board, before
/// any network work. Pure (testable): drone profile, not `off`, and either forced
/// on or NPU-less (`auto` offloads only when there is no local accelerator).
fn should_attempt(profile: &str, offload_off: bool, offload_forced: bool, npu_tops: f64) -> bool {
    if profile != "drone" || offload_off {
        return false;
    }
    offload_forced || npu_tops <= 0.0
}

/// Split a `host:port` (or bare `host`) into `(host, port)`, defaulting the port.
/// Pure (testable). An IPv6 literal in brackets keeps its colons.
fn split_host_port(s: &str, default_port: u16) -> Option<(String, u16)> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // `[::1]:8092` — bracketed IPv6 with an optional port.
    if let Some(rest) = s.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        let port = after
            .strip_prefix(':')
            .and_then(|p| p.parse().ok())
            .unwrap_or(default_port);
        return Some((host.to_string(), port));
    }
    match s.rsplit_once(':') {
        // A single colon = host:port; more than one and unbracketed = a bare IPv6.
        Some((h, p)) if !h.contains(':') => {
            let port = p.parse().ok().unwrap_or(default_port);
            Some((h.to_string(), port))
        }
        _ => Some((s.to_string(), default_port)),
    }
}

/// The drone's egress IP toward `host:port` — the source address a connection to
/// the node would use, i.e. the address the node can reach the drone's RTSP feed
/// back on. A UDP "connect" sets the default route + picks the source IP without
/// sending a packet. `None` when the node address doesn't resolve or no route
/// exists (Rule 47: no verified reach ⇒ don't advertise one).
fn local_ip_towards(host: &str, port: u16) -> Option<IpAddr> {
    let addr = format!("{host}:{port}").to_socket_addrs().ok()?.next()?;
    let sock = UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    sock.connect(addr).ok()?;
    sock.local_addr().ok().map(|a| a.ip())
}

/// The board's declared NPU throughput, from the board sidecar (0.0 when absent /
/// unreadable / no NPU).
fn board_npu_tops() -> f64 {
    std::fs::read_to_string(BOARD_JSON)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("npu_tops").and_then(|x| x.as_f64()))
        .unwrap_or(0.0)
}

/// Whether the camera pipeline is ready (a live primary camera) — the node can
/// only pull frames the drone is actually publishing.
fn camera_ready() -> bool {
    std::fs::read_to_string(CAMERA_STATE_JSON)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| {
            v.get("state")
                .and_then(|s| s.as_str())
                .map(|s| s == "ready")
        })
        .unwrap_or(false)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Resolve the target node: a pinned `perception.offload.compute_node_addr` (skip
/// mDNS), else browse mDNS for a `profile=workstation` node. Returns
/// `(host, port, device_id?)`.
async fn resolve_node(config: &CloudConfig) -> Option<(String, u16, Option<String>)> {
    if let Some(pinned) = perception_offload_addr(config) {
        let (h, p) = split_host_port(&pinned, DEFAULT_JOB_API_PORT)?;
        return Some((h, p, None));
    }
    let node = resolve_compute(RESOLVE_TIMEOUT).await?;
    Some((node.host, node.job_api_port, Some(node.device_id)))
}

/// Build a concrete offload decision (or Idle) — the network-touching resolve of
/// the pure gate. `may_resolve` throttles the mDNS browse while searching.
async fn decide(config: &CloudConfig, may_resolve: bool) -> Decision {
    if !should_attempt(
        &config.agent.profile,
        config.perception.offload.is_off(),
        config.perception.offload.is_forced_on(),
        board_npu_tops(),
    ) {
        return Decision::Idle;
    }
    if !camera_ready() || !may_resolve {
        return Decision::Idle;
    }
    let Some((node_host, node_port, node_device_id)) = resolve_node(config).await else {
        return Decision::Idle;
    };
    let Some(local_ip) = local_ip_towards(&node_host, node_port) else {
        return Decision::Idle;
    };
    Decision::Offload {
        base_url: format!("http://{node_host}:{node_port}"),
        target: format!("{node_host}:{node_port}"),
        node_device_id,
        rtsp_url: format!("rtsp://{local_ip}:{RTSP_PORT}/{RTSP_PATH}"),
        width: config.video.camera.width,
        height: config.video.camera.height,
    }
}

/// Write (or clear) the offload-link sidecar. `bearer_acceptable` mirrors `paired`
/// — a resolved, reachable node is treated as an acceptable LAN bearer (a real
/// latency-budget probe is a future refinement); this is honest reachability, not
/// a fabricated link.
fn write_link(paired: bool, target: Option<String>, device_id: Option<String>) {
    let link = OffloadLink::stamped(
        paired,
        paired,
        target,
        device_id,
        Some("offload".to_string()),
        now_ms(),
    );
    if let Err(e) = write_offload_link(&link) {
        tracing::debug!(error = %e, "write offload-link sidecar");
    }
}

fn session_id(config: &CloudConfig) -> String {
    format!("offload-{}", config.agent.device_id)
}

/// Run the offload reconciler until `shutdown` flips.
pub async fn run(config: Arc<CloudConfig>, mut shutdown: watch::Receiver<bool>) {
    // Cheap inert gate (config is loaded once at daemon start, changed via a
    // restart): a non-drone or an opted-out drone does no offload work.
    if config.agent.profile != "drone" || config.perception.offload.is_off() {
        return;
    }
    tracing::info!(
        "offload reconciler: armed (auto-offload when NPU-less + a workstation is reachable)"
    );

    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut running: Option<RunningSession> = None;
    let mut last_search: Option<Instant> = None;

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { break; }
            }
            _ = ticker.tick() => {
                // A finished task (the node went away / the stream ended) frees
                // the slot so the next tick re-resolves.
                if running.as_ref().is_some_and(|s| s.handle.is_finished()) {
                    running = None;
                }

                // Throttle mDNS browses while searching (no active session); when
                // a session is running we never re-resolve.
                let may_resolve = if running.is_some() {
                    false
                } else {
                    let due = last_search.is_none_or(|t| t.elapsed() >= SEARCH_BACKOFF);
                    if due { last_search = Some(Instant::now()); }
                    due
                };

                match decide(&config, may_resolve).await {
                    Decision::Offload { base_url, target, node_device_id, rtsp_url, width, height } => {
                        if running.is_none() {
                            let cancel = Arc::new(Notify::new());
                            let cfg = OrchestratorConfig::vision_only(
                                session_id(&config), CAMERA_ID, rtsp_url, width, height, TARGET_BUDGET_MS,
                            );
                            let api_key = PairingState::load().api_key().map(str::to_string);
                            let endpoint = NodeEndpoint::Direct { base_url, api_key };
                            let cancel_task = cancel.clone();
                            let target_log = target.clone();
                            tracing::info!(target = %target_log, "offload reconciler: starting a session");
                            let handle = tokio::spawn(async move {
                                if let Err(e) = run_offload_orchestrator(cfg, endpoint, cancel_task).await {
                                    tracing::warn!(error = %e, "offload orchestrator ended");
                                }
                            });
                            running = Some(RunningSession { cancel, handle, target: target.clone() });
                        }
                        // Refresh the link each tick so its mtime stays fresh.
                        write_link(true, Some(target), node_device_id);
                    }
                    Decision::Idle => {
                        if let Some(sess) = running.take() {
                            tracing::info!(target = %sess.target, "offload reconciler: stopping the session");
                            sess.cancel.notify_waiters();
                            write_link(false, None, None);
                        }
                    }
                }
            }
        }
    }

    if let Some(sess) = running.take() {
        sess.cancel.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_attempt_only_on_a_drone() {
        // NPU-less drone on auto ⇒ attempt.
        assert!(should_attempt("drone", false, false, 0.0));
        // A workstation never offloads.
        assert!(!should_attempt("workstation", false, false, 0.0));
        // A ground station never offloads.
        assert!(!should_attempt("ground-station", false, false, 0.0));
    }

    #[test]
    fn auto_offloads_only_when_npu_less() {
        // auto + an accelerator ⇒ run local, do not offload.
        assert!(!should_attempt("drone", false, false, 6.0));
        // auto + no accelerator ⇒ offload.
        assert!(should_attempt("drone", false, false, 0.0));
        // forced on ⇒ offload even with an accelerator.
        assert!(should_attempt("drone", false, true, 6.0));
        // off ⇒ never, even NPU-less.
        assert!(!should_attempt("drone", true, false, 0.0));
    }

    #[test]
    fn split_host_port_defaults_and_parses() {
        assert_eq!(
            split_host_port("192.168.1.5:9000", 8092),
            Some(("192.168.1.5".into(), 9000))
        );
        assert_eq!(
            split_host_port("192.168.1.5", 8092),
            Some(("192.168.1.5".into(), 8092))
        );
        assert_eq!(split_host_port("  ", 8092), None);
        // A bare IPv6 keeps its colons + takes the default port.
        assert_eq!(
            split_host_port("fe80::1", 8092),
            Some(("fe80::1".into(), 8092))
        );
        // A bracketed IPv6 with a port.
        assert_eq!(
            split_host_port("[fe80::1]:9000", 8092),
            Some(("fe80::1".into(), 9000))
        );
    }

    #[test]
    fn local_ip_towards_a_public_ip_is_some() {
        // A route to a public IP exists in any networked test env; the call sends
        // no packet, it only picks the source address.
        assert!(local_ip_towards("1.1.1.1", 80).is_some());
        // An unresolvable host yields None (never a fabricated reach).
        assert!(local_ip_towards("not a host", 80).is_none());
    }
}
