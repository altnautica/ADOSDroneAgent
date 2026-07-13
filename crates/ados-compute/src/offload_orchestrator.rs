//! The drone-side offload orchestrator: the auto-drive half of the offload path.
//!
//! When the agent picks the `offload` perception tier (an NPU-less / too-slow
//! board with a paired workstation node — see `ados_offload::pick_tier`), this
//! orchestrator wires the whole lane in one call:
//!
//! 1. **discover** the paired compute node (mDNS `profile=workstation`), or use an
//!    injected node URL (a node reached by a known IP, and the test seam);
//! 2. **submit** a streaming perception-offload session job to the node
//!    ([`ComputeClient::submit_job`]) naming the drone's live RTSP feed — the node
//!    starts the session (see [`crate::offload_session_manager`]);
//! 3. **open** the node's per-session detection WebSocket
//!    ([`stream_offload_detections`]);
//! 4. **drain** each returned batch through the [`OffloadReturnBridge`] — which
//!    wraps the `ados_offload` freshness + lock safety gate — and republish onto
//!    the drone's local `vision.detection` bus ([`VisionSockPublisher`]).
//!
//! Safety (the whole reason `ados-offload` exists): the bridge runs a periodic
//! tick so a stalled stream or a dropped link trips the designated track's lock
//! to `Lost` on the local clock — a returned box is never extrapolated and a
//! dropped lock never auto-re-acquires. When the lane closes, the link is marked
//! down and settled to `Lost` before the orchestrator returns.
//!
//! This is the entry point a future supervisor wiring calls on a live tier flip
//! (`perceptionTier == offload` + a paired node). That auto-wire — reading the
//! real tier signal + the real paired-node handle — is the W8 rig step and is
//! deliberately NOT done here: shipping an untested live auto-trigger would
//! violate verify-don't-assert. This pass ships the buildable, SITL-tested entry
//! point + its config.

use std::sync::Arc;
use std::time::Duration;

use ados_offload::OffloadMode;
use ados_protocol::offload::OffloadDetectionBatch;
use anyhow::{anyhow, Result};
use tokio::sync::{mpsc, Notify};

use crate::offload_bridge::{OffloadReturnBridge, VisionSockPublisher};
use crate::offload_client::stream_offload_detections;
use crate::{ComputeClient, ComputeJobKind};

/// The detection channel buffer between the WS subscriber and the drain loop.
const RETURN_CHANNEL_CAP: usize = 64;
/// How often the safety gate is advanced without a new batch, so a stalled
/// stream or a dropped link trips the lock to `Lost` promptly on the local clock.
const SAFETY_TICK_MS: u64 = 100;
/// The default `/run/ados/vision.sock` the return bridge publishes onto.
pub const DEFAULT_VISION_SOCK: &str = "/run/ados/vision.sock";

/// Where to reach the compute node.
pub enum NodeEndpoint {
    /// Discover a paired `profile=workstation` node over mDNS (production). The
    /// `api_key` rides `X-ADOS-Key` for the off-box leg (`None` on-box/unpaired).
    Discover {
        timeout: Duration,
        api_key: Option<String>,
    },
    /// A pre-resolved node — reached by a known base URL (`http://host:8092`). The
    /// production path for a node added by IP, and the SITL seam (skip discovery).
    Direct {
        base_url: String,
        api_key: Option<String>,
    },
}

/// The orchestrator's configuration: the session identity, the drone's live feed,
/// and the safety budgets the return bridge gates on.
pub struct OrchestratorConfig {
    /// The session id (the WS path + the batch tag; the idempotent job id).
    pub session_id: String,
    /// The camera the detections are attributed to (tags each batch).
    pub camera_id: String,
    /// The drone's live RTSP feed the node pulls (e.g. `rtsp://localhost:8554/main`).
    pub rtsp_url: String,
    /// The camera's frame size, so the node decodes fixed RGB24 frames.
    pub width: u32,
    pub height: u32,
    /// What the node returns (detections / poses / both).
    pub mode: OffloadMode,
    /// The freshness budget (ms) for the detection stream — past this with no new
    /// batch, the gate trips the lock to `Lost`.
    pub target_budget_ms: i64,
    /// The freshness budget (ms) for the pose stream (matters only for
    /// `SlamOnly`/`Full`).
    pub pose_budget_ms: i64,
    /// The model id stamped on the republished batch (labels the offload source).
    pub model_id: String,
    /// The vision request socket the bridge publishes onto (defaults to
    /// [`DEFAULT_VISION_SOCK`]).
    pub vision_sock: String,
}

impl OrchestratorConfig {
    /// A vision-only offload of `camera_id`'s `rtsp_url` feed under `session_id`,
    /// with a `target_budget_ms` freshness budget. The pose budget mirrors it
    /// (unused for `VisionOnly`) and the vision socket defaults to the standard
    /// path.
    pub fn vision_only(
        session_id: impl Into<String>,
        camera_id: impl Into<String>,
        rtsp_url: impl Into<String>,
        width: u32,
        height: u32,
        target_budget_ms: i64,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            camera_id: camera_id.into(),
            rtsp_url: rtsp_url.into(),
            width,
            height,
            mode: OffloadMode::VisionOnly,
            target_budget_ms,
            pose_budget_ms: target_budget_ms,
            model_id: "offload".into(),
            vision_sock: DEFAULT_VISION_SOCK.into(),
        }
    }
}

/// Derive the node's base URL (and off-box key) from a [`NodeEndpoint`], resolving
/// mDNS when asked. Fails when discovery finds no paired workstation node.
async fn resolve_node(node: NodeEndpoint) -> Result<(String, Option<String>)> {
    match node {
        NodeEndpoint::Direct { base_url, api_key } => Ok((base_url, api_key)),
        NodeEndpoint::Discover { timeout, api_key } => {
            let resolved = crate::mdns::resolve_compute(timeout)
                .await
                .ok_or_else(|| anyhow!("no paired compute node discovered on the LAN"))?;
            Ok((
                format!("http://{}:{}", resolved.host, resolved.job_api_port),
                api_key,
            ))
        }
    }
}

/// Build the per-session detection WS URL from the node's base URL: the WS router
/// is mounted on the node's job-API listener, so `http(s)://host:port` →
/// `ws(s)://host:port/ws/offload/<session>`.
fn ws_url_from_base(base_url: &str, session_id: &str) -> String {
    let base = base_url.trim_end_matches('/');
    let ws_base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_string()
    };
    format!("{ws_base}/ws/offload/{session_id}")
}

/// Run the offload orchestrator to completion: submit the session, subscribe to
/// the node's detection return stream, and republish onto the local vision bus
/// through the safety gate until `cancel` fires or the node stream ends.
///
/// The session identity travels in the job params (`session.id`); the node's
/// active-session map is the source of truth for dedup, so the job row is just an
/// ephemeral trigger with a node-minted id. A re-open after a session ended always
/// mints a fresh trigger the worker picks up and (re)starts the session — a
/// retained terminal job from a prior open never blocks the restart — while a
/// re-submit of a still-live session is deduped on the node (a harmless no-op). A
/// hard submit failure returns an error so the caller retries; the WS is not opened
/// against a node that never accepted the trigger.
pub async fn run_offload_orchestrator(
    cfg: OrchestratorConfig,
    node: NodeEndpoint,
    cancel: Arc<Notify>,
) -> Result<()> {
    let (base_url, api_key) = resolve_node(node).await?;

    // 1 + 2: submit the streaming-session job so the node starts the session.
    let client = ComputeClient::new(base_url.clone(), api_key);
    let params = serde_json::json!({
        "session": {
            "id": cfg.session_id,
            "rtsp_url": cfg.rtsp_url,
            "camera_id": cfg.camera_id,
            "width": cfg.width,
            "height": cfg.height,
        }
    });
    match client
        .submit_job(
            // No dataset (the session consumes the live RTSP feed, not a stored
            // dataset) and no caller-chosen job id (the node mints a unique one, so
            // a re-open is never swallowed as a duplicate of a retained job).
            ComputeJobKind::PerceptionOffload,
            None,
            params,
            None,
        )
        .await
    {
        Ok(_) => {
            tracing::info!(session = %cfg.session_id, node = %base_url, "offload session submitted")
        }
        Err(e) => return Err(anyhow!("submit offload session job: {e}")),
    }

    // 3: open the node's per-session detection WS.
    let ws_url = ws_url_from_base(&base_url, &cfg.session_id);
    let (det_tx, det_rx) = mpsc::channel::<OffloadDetectionBatch>(RETURN_CHANNEL_CAP);
    let stream_cancel = cancel.clone();
    let ws = ws_url.clone();
    let subscriber = tokio::spawn(async move {
        if let Err(e) = stream_offload_detections(&ws, det_tx, stream_cancel).await {
            tracing::warn!(url = %ws, error = %e, "offload detection stream error");
        }
    });

    // 4: drain returned batches through the safety gate onto the local bus.
    drain_into_bridge(&cfg, det_rx, cancel).await;

    // The stream task is cancelled with the same handle (or already ended when
    // its sink closed); reap it.
    let _ = subscriber.await;
    Ok(())
}

/// Drain the return stream into the bridge and republish onto the local vision
/// bus, advancing the safety gate on a periodic tick so a stalled stream trips
/// the lock to `Lost`. Returns when the stream ends or `cancel` fires; settles
/// the gate to link-down before returning.
async fn drain_into_bridge(
    cfg: &OrchestratorConfig,
    mut det_rx: mpsc::Receiver<OffloadDetectionBatch>,
    cancel: Arc<Notify>,
) {
    let mut bridge = OffloadReturnBridge::new(
        cfg.mode,
        cfg.target_budget_ms,
        cfg.pose_budget_ms,
        cfg.model_id.clone(),
    );
    let mut publisher = VisionSockPublisher::new(cfg.vision_sock.clone());

    let mut safety_tick = tokio::time::interval(Duration::from_millis(SAFETY_TICK_MS));
    safety_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Hold one notified future for the whole loop so a cancel fired between
    // iterations is never missed (mirrors run_offload_session).
    let cancelled = cancel.notified();
    tokio::pin!(cancelled);
    loop {
        tokio::select! {
            maybe = det_rx.recv() => match maybe {
                Some(batch) => {
                    let (db, _status) = bridge.ingest(&batch, now_ms());
                    // A publish fault drops + reconnects the socket on the next
                    // batch; a returned box is never dropped silently past a log.
                    if let Err(e) = publisher.publish(&db).await {
                        tracing::warn!(error = %e, "offload return publish onto vision bus failed");
                    }
                }
                // The node stream ended (session stopped / node gone) or the
                // subscriber closed its sink: the lane is done.
                None => break,
            },
            _ = safety_tick.tick() => {
                // Advance the gate with no new batch so a stalled stream / dropped
                // link trips the designated lock to Lost on the local clock. Never
                // extrapolates a stale box.
                let _ = bridge.tick(now_ms());
            }
            _ = &mut cancelled => break,
        }
    }

    // The lane is closing: mark the link down and settle the gate to Lost so no
    // stale box is left commanding and the lock never auto-re-acquires.
    bridge.set_link(false);
    let _ = bridge.tick(now_ms());
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_maps_http_to_ws_and_https_to_wss() {
        assert_eq!(
            ws_url_from_base("http://127.0.0.1:8092", "s1"),
            "ws://127.0.0.1:8092/ws/offload/s1"
        );
        assert_eq!(
            ws_url_from_base("https://node.local:8092/", "abc"),
            "wss://node.local:8092/ws/offload/abc"
        );
        // A bare host:port (no scheme) is left as-is with the path appended.
        assert_eq!(
            ws_url_from_base("node.local:8092", "s2"),
            "node.local:8092/ws/offload/s2"
        );
    }

    #[tokio::test]
    async fn direct_endpoint_resolves_to_its_base_url() {
        let (base, key) = resolve_node(NodeEndpoint::Direct {
            base_url: "http://10.0.0.5:8092".into(),
            api_key: Some("k".into()),
        })
        .await
        .unwrap();
        assert_eq!(base, "http://10.0.0.5:8092");
        assert_eq!(key.as_deref(), Some("k"));
    }

    #[test]
    fn vision_only_config_defaults_the_socket_and_model() {
        let cfg = OrchestratorConfig::vision_only(
            "s1",
            "front",
            "rtsp://localhost:8554/main",
            640,
            480,
            1000,
        );
        assert_eq!(cfg.mode, OffloadMode::VisionOnly);
        assert_eq!(cfg.vision_sock, DEFAULT_VISION_SOCK);
        assert_eq!(cfg.model_id, "offload");
        assert_eq!(cfg.pose_budget_ms, 1000);
    }
}
