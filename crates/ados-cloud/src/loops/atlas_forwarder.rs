//! The drone-side Atlas forwarder.
//!
//! The Atlas capture service (`ados-atlas`) publishes pose-tagged keyframes, the
//! live pose, and capture state onto the local atlas bus (`/run/ados/atlas.sock`
//! — one msgpack-framed [`AtlasEvent`] per broadcast). Those artifacts have to
//! leave the drone for a compute node to reconstruct a world model from. This
//! loop is that egress: it SUBSCRIBES to the atlas bus and forwards every event
//! over the bearer ladder — direct LAN first, then the WFB relay for the field,
//! then the opt-in cloud lane — local-first (Rule 39).
//!
//! The compute node is discovered over mDNS (a service advertising
//! `profile=workstation`); its job-API base URL backs the direct-LAN bearer.
//! When the LAN bearer stops carrying (the node went away, or the ladder fell
//! over to a slower lane), the loop re-resolves and rebuilds the ladder; while
//! no LAN bearer is present it periodically re-browses so a node that boots after
//! the drone is still picked up.
//!
//! INERT by default: it early-returns unless Atlas is enabled
//! ([`crate::config::CloudConfig::atlas_enabled`]), so a non-Atlas agent does no
//! Atlas work and is byte-unchanged.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::watch;

use ados_atlas_transport::{
    AtlasBearer, AtlasEvent, BearerKind, BearerLadder, LanHttpBearer, WfbRelayBearer,
};
use ados_protocol::atlas::{
    AtlasForwardStatus, ATLAS_FORWARD_SIDECAR, ATLAS_FORWARD_SIDECAR_VERSION, ATLAS_KEYFRAME_TOPIC,
};
use ados_protocol::frame::PLUGIN_MAX_FRAME;
use ados_protocol::ipc::{connect_with_retry, read_length_prefixed};

use crate::atlas_bearer::CloudBearer;
use crate::config::CloudConfig;
use crate::mqtt::transport::{RumqttcTransport, TransportConfig};
use crate::mqtt::WS_PATH;
use crate::pairing::PairingState;

/// The local atlas bus the capture service publishes onto.
const ATLAS_SOCK: &str = "/run/ados/atlas.sock";
/// How long one mDNS browse waits for a compute node to answer.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(3);
/// Backoff before reconnecting the atlas bus after an EOF / read error.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);
/// While no direct-LAN bearer is present, re-browse for a compute node no more
/// often than this so a node that boots after the drone is still picked up.
const RERESOLVE_INTERVAL: Duration = Duration::from_secs(30);
/// Consecutive sends that did NOT ride the LAN bearer (it fell over, or errored)
/// before the LAN bearer is treated as gone and the node is re-resolved.
const LAN_MISS_THRESHOLD: u32 = 5;
/// Connect-retry budget for the atlas bus: the capture service may still be
/// binding the socket on a cold boot, so the subscriber retries rather than
/// failing fast.
const CONNECT_RETRIES: u32 = 30;
const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(500);
/// The in-flight ceiling for the dedicated Atlas cloud session (matches the
/// MAVLink relay's Rule-37 high ceiling — the publish path is the limit).
const CLOUD_INFLIGHT: u16 = 1000;
const CLOUD_KEEP_ALIVE: Duration = Duration::from_secs(30);
/// While a compute node is resolved, re-write the forwarder handoff at least this
/// often so its mtime stays fresh (the capture service drops a stale handoff so a
/// gone node never lingers on the Stream card). Comfortably under the reader's
/// staleness window.
const FORWARD_REFRESH: Duration = Duration::from_secs(5);

/// Outcome of one forward attempt.
enum Forwarded {
    /// The event decoded; `carried` is `Some(kind)` if a bearer carried it (`None`
    /// if every bearer declined / errored), and `keyframe` is whether it was a
    /// keyframe (so the handoff can stamp the last-forwarded-keyframe time).
    Decoded {
        carried: Option<BearerKind>,
        keyframe: bool,
    },
    /// The framed body was not a valid event — logged, no send attempted (does
    /// NOT count as a transport miss, so it never re-resolves the node).
    DecodeError,
}

/// Epoch milliseconds.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Map a bearer kind to the GCS Stream-card vocabulary the handoff carries.
fn bearer_label(kind: BearerKind) -> &'static str {
    match kind {
        // Loopback / bulk are same-host LAN-direct-ish; the live ladder only ever
        // carries over DirectLan / WfbRelay / Cloud on real hardware.
        BearerKind::DirectLan | BearerKind::Loopback | BearerKind::PostFlightBulk => "direct-lan",
        BearerKind::WfbRelay => "wfb-relay",
        BearerKind::Cloud => "cloud",
    }
}

/// The transport facts only the egress forwarder knows, written to the handoff
/// file for the capture service to fold into `atlas-state.json` (so the GCS
/// Stream card reads real values). Written on change, and periodically while a
/// node is resolved so the reader's freshness gate never drops a live node.
#[derive(Default)]
struct ForwardStatus {
    compute_node_id: Option<String>,
    bearer: Option<String>,
    last_kf_at_ms: Option<i64>,
    last_write: Option<Instant>,
}

impl ForwardStatus {
    /// Record the resolved compute node (or its loss). Returns whether it changed.
    /// An empty advertised id counts as none; losing the node clears the bearer
    /// (nothing is carrying anymore).
    fn note_node(&mut self, node_id: Option<String>) -> bool {
        let node_id = node_id.filter(|s| !s.is_empty());
        if node_id == self.compute_node_id {
            return false;
        }
        if node_id.is_none() {
            self.bearer = None;
        }
        self.compute_node_id = node_id;
        true
    }

    /// Record a forwarded event's carrying bearer (+ keyframe time). Returns
    /// whether an operator-visible fact moved (the bearer changed, or a keyframe
    /// was forwarded). A declined send (`carried = None`) keeps the last-known
    /// bearer — a transient decline should not flicker it; a persistent one lets
    /// the handoff age out.
    fn note_carried(&mut self, bearer: Option<&'static str>, keyframe: bool, now_ms: i64) -> bool {
        let mut changed = false;
        if let Some(b) = bearer {
            if self.bearer.as_deref() != Some(b) {
                self.bearer = Some(b.to_string());
                changed = true;
            }
        }
        if keyframe {
            self.last_kf_at_ms = Some(now_ms);
            changed = true;
        }
        changed
    }

    /// Write the handoff when a fact changed, or periodically while a node is
    /// resolved to keep the file's mtime fresh. Best-effort.
    fn maybe_write(&mut self, dirty: bool) {
        let due = self.compute_node_id.is_some()
            && self
                .last_write
                .map(|t| t.elapsed() >= FORWARD_REFRESH)
                .unwrap_or(true);
        if !dirty && !due {
            return;
        }
        write_forward_status(&AtlasForwardStatus {
            version: ATLAS_FORWARD_SIDECAR_VERSION,
            compute_node_id: self.compute_node_id.clone(),
            bearer: self.bearer.clone(),
            last_kf_at_ms: self.last_kf_at_ms,
            generated_at_ms: now_ms(),
        });
        self.last_write = Some(Instant::now());
    }
}

/// Atomically write the forwarder handoff (`.tmp` + rename, so the capture
/// service never reads a torn file). Best-effort: a write error is logged.
fn write_forward_status(status: &AtlasForwardStatus) {
    let path = Path::new(ATLAS_FORWARD_SIDECAR);
    let body = match serde_json::to_vec(status) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "atlas_forward_encode_failed");
            return;
        }
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("json.tmp");
    if let Err(e) = std::fs::write(&tmp, &body).and_then(|()| std::fs::rename(&tmp, path)) {
        tracing::debug!(error = %e, "atlas_forward_write_failed");
    }
}

/// Run the Atlas forwarder until `shutdown` flips. INERT unless Atlas is enabled.
pub async fn run(config: Arc<CloudConfig>, mut shutdown: watch::Receiver<bool>) {
    // INERT: a non-Atlas agent does no work and is byte-unchanged.
    if !config.atlas_enabled() {
        tracing::debug!("atlas forwarder idle: atlas is not enabled");
        return;
    }
    tracing::info!("atlas forwarder starting");

    // The cloud transport is built at most once and reused across ladder
    // rebuilds, so re-resolving the LAN node never churns a new MQTT session
    // (a second session on the same client id would kick the first). `None`
    // until cloud relay is the posture AND the agent is paired.
    let mut cloud_transport: Option<Arc<RumqttcTransport>> = None;

    // The forwarder handoff (compute node + bearer + last-keyframe time) the
    // capture service folds into its plugin-state sidecar. Persists across bus
    // reconnects so a blip never drops the resolved node from the Stream card.
    let mut fwd = ForwardStatus::default();

    loop {
        if *shutdown.borrow() {
            break;
        }

        // (Re)connect the atlas bus subscriber.
        let mut stream =
            match connect_with_retry(ATLAS_SOCK, CONNECT_RETRIES, CONNECT_RETRY_DELAY).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(error = %e, "atlas bus not reachable; retrying");
                    if sleep_or_shutdown(&mut shutdown, RECONNECT_DELAY).await {
                        break;
                    }
                    continue;
                }
            };
        tracing::info!(socket = ATLAS_SOCK, "atlas forwarder subscribed to the bus");

        // Resolve the compute node + build the ladder. `have_lan` tracks whether
        // a direct-LAN bearer is currently present; the resolved node id feeds the
        // forwarder handoff so the Stream card names the reconstructor.
        let (mut ladder, node_id) = build_ladder(&config, &mut cloud_transport).await;
        let mut have_lan = node_id.is_some();
        let node_changed = fwd.note_node(node_id);
        fwd.maybe_write(node_changed);
        let mut lan_misses: u32 = 0;
        let mut last_resolve = Instant::now();

        // Drain events until the socket closes or shutdown. The read is raced
        // ONLY against shutdown (which ends the whole loop), so a partial read is
        // never cancelled mid-frame by a timer — that would desync the stream.
        loop {
            let read = tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { return; }
                    continue;
                }
                r = read_length_prefixed(&mut stream, PLUGIN_MAX_FRAME, false) => r,
            };

            match read {
                Ok(Some(body)) => {
                    let mut dirty = false;
                    match forward_event(&ladder, &body, &config.agent.device_id).await {
                        Forwarded::Decoded { carried, keyframe } => {
                            if have_lan {
                                if carried == Some(BearerKind::DirectLan) {
                                    lan_misses = 0;
                                } else {
                                    lan_misses += 1;
                                }
                            }
                            dirty |=
                                fwd.note_carried(carried.map(bearer_label), keyframe, now_ms());
                        }
                        Forwarded::DecodeError => {}
                    }

                    // Re-resolve when the LAN bearer stopped carrying, or
                    // periodically while we have no LAN bearer at all (a compute
                    // node may have appeared). Done between complete frames so it
                    // never cancels a partial read.
                    let lan_lost = have_lan && lan_misses >= LAN_MISS_THRESHOLD;
                    let want_discover = !have_lan && last_resolve.elapsed() >= RERESOLVE_INTERVAL;
                    if lan_lost || want_discover {
                        if lan_lost {
                            tracing::info!(
                                "atlas LAN bearer stopped carrying; re-resolving compute node"
                            );
                        }
                        let (l, node_id) = build_ladder(&config, &mut cloud_transport).await;
                        let lan = node_id.is_some();
                        if lan && !have_lan {
                            tracing::info!("atlas forwarder discovered a compute node");
                        }
                        dirty |= fwd.note_node(node_id);
                        ladder = l;
                        have_lan = lan;
                        lan_misses = 0;
                        last_resolve = Instant::now();
                    }

                    // Persist the handoff (on change, or the periodic mtime refresh).
                    fwd.maybe_write(dirty);
                }
                Ok(None) => {
                    tracing::debug!("atlas bus closed; reconnecting");
                    break;
                }
                Err(e) => {
                    tracing::debug!(error = %e, "atlas bus read error; reconnecting");
                    break;
                }
            }
        }

        if sleep_or_shutdown(&mut shutdown, RECONNECT_DELAY).await {
            break;
        }
    }
    tracing::info!("atlas forwarder stopped");
}

/// Decode one framed [`AtlasEvent`] body, stamp the capturing drone's device id,
/// and send it over the ladder. Returns the bearer that carried it (`Some`), or
/// that every bearer declined (`None`); a malformed body is logged and reported
/// as a decode error (no send). Never panics, never propagates — a forward
/// failure is logged and the loop continues.
///
/// The device id is stamped HERE, the single egress choke point every bearer
/// passes through, so the compute node can attribute the reconstruct job to the
/// drone that captured it. An empty configured id leaves the event unstamped
/// (never writes an empty attribution).
async fn forward_event(ladder: &BearerLadder, body: &[u8], device_id: &str) -> Forwarded {
    let mut event = match AtlasEvent::decode(body) {
        Ok(ev) => ev,
        Err(e) => {
            tracing::warn!(error = %e, "atlas forwarder dropped a malformed event");
            return Forwarded::DecodeError;
        }
    };
    if !device_id.is_empty() {
        event.device_id = Some(device_id.to_string());
    }
    let keyframe = event.topic == ATLAS_KEYFRAME_TOPIC;
    match ladder.send(&event).await {
        Ok(kind) => {
            tracing::debug!(
                topic = %event.topic,
                bearer = ?kind,
                bytes = event.payload.len(),
                "atlas event forwarded"
            );
            Forwarded::Decoded {
                carried: Some(kind),
                keyframe,
            }
        }
        Err(e) => {
            tracing::debug!(
                topic = %event.topic,
                error = %e,
                "atlas event not forwarded (all bearers declined)"
            );
            Forwarded::Decoded {
                carried: None,
                keyframe,
            }
        }
    }
}

/// Resolve the compute node and build the bearer ladder. Returns the ladder and
/// the resolved compute node's device id (`Some` iff a direct-LAN bearer is
/// present — a compute node was resolved). The cloud transport is built at most
/// once (the first time cloud relay is the posture AND the agent is paired) and
/// reused on every rebuild.
async fn build_ladder(
    config: &CloudConfig,
    cloud_transport: &mut Option<Arc<RumqttcTransport>>,
) -> (BearerLadder, Option<String>) {
    let mut bearers: Vec<Box<dyn AtlasBearer>> = Vec::new();

    // ── Direct LAN (first-class): a resolved compute node's job-API URL ──
    let compute_node_id = match ados_compute::mdns::resolve_compute(RESOLVE_TIMEOUT).await {
        Some(node) => {
            let base = format!("http://{}:{}", node.host, node.job_api_port);
            tracing::info!(base = %base, node = %node.device_id, "atlas forwarder resolved compute node");
            bearers.push(Box::new(LanHttpBearer::new(base)));
            Some(node.device_id)
        }
        None => {
            tracing::debug!("atlas forwarder: no compute node on mDNS yet");
            None
        }
    };

    // ── WFB relay (field): the ground agent bridges WFB<->LAN. The bearer is
    //    available only when the aux radio stream is provisioned; otherwise the
    //    ladder skips it. ──
    bearers.push(Box::new(WfbRelayBearer::new()));

    // ── Cloud (opt-in, last rung): only when cloud relay is the posture ──
    if config.cloud_relay_enabled() {
        if cloud_transport.is_none() {
            *cloud_transport = build_cloud_transport(config);
        }
        if let Some(t) = cloud_transport.as_ref() {
            bearers.push(Box::new(CloudBearer::new(
                config.agent.device_id.clone(),
                t.clone(),
            )));
        }
    }

    (BearerLadder::new(bearers), compute_node_id)
}

/// Build the dedicated Atlas cloud transport, or `None` while unpaired. It uses
/// a DISTINCT client id (`ados-{device}-atlas`) so it never collides with the
/// MAVLink relay's `ados-{device}` session (a same-id second session kicks the
/// first); the username + key are the device's own, so the broker ACL still
/// authorizes its `ados/{device}/atlas/*` topics.
fn build_cloud_transport(config: &CloudConfig) -> Option<Arc<RumqttcTransport>> {
    let pairing = PairingState::load();
    let api_key = pairing.api_key()?;
    let cfg = TransportConfig {
        client_id: format!("ados-{}-atlas", config.agent.device_id),
        host: config.server.cloud.mqtt_broker.clone(),
        port: config.server.cloud.mqtt_port,
        ws_path: WS_PATH.to_string(),
        username: format!("ados-{}", config.agent.device_id),
        password: api_key.to_string(),
        inflight: CLOUD_INFLIGHT,
        keep_alive: CLOUD_KEEP_ALIVE,
    };
    tracing::info!("atlas forwarder cloud lane connecting");
    Some(RumqttcTransport::connect(&cfg))
}

/// Sleep for `delay` unless shutdown flips first. Returns `true` if shutdown was
/// requested (the caller should break its loop).
async fn sleep_or_shutdown(shutdown: &mut watch::Receiver<bool>, delay: Duration) -> bool {
    tokio::select! {
        _ = shutdown.changed() => *shutdown.borrow(),
        _ = tokio::time::sleep(delay) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_atlas_transport::LoopbackBearer;

    #[tokio::test]
    async fn forward_event_carries_a_decoded_event_over_the_ladder() {
        let (bearer, mut rx) = LoopbackBearer::channel();
        let ladder = BearerLadder::new(vec![Box::new(bearer)]);
        let ev = AtlasEvent::new(ATLAS_KEYFRAME_TOPIC, None, vec![1, 2, 3]);
        let body = ev.encode().unwrap();

        match forward_event(&ladder, &body, "drone-7").await {
            // A keyframe carried over the loopback bearer, flagged as a keyframe so
            // the handoff can stamp the last-forwarded-keyframe time.
            Forwarded::Decoded {
                carried: Some(BearerKind::Loopback),
                keyframe: true,
            } => {}
            other => panic!(
                "expected a loopback-carried keyframe, got {:?}",
                carried_kind(&other)
            ),
        }
        // The event arrived stamped with the capturing drone's device id (added at
        // the egress choke point), and is otherwise the exact envelope.
        let got = rx
            .try_recv()
            .expect("the loopback bearer carried the event");
        assert_eq!(got.topic, ev.topic);
        assert_eq!(got.payload, ev.payload);
        assert_eq!(got.device_id.as_deref(), Some("drone-7"));
    }

    #[tokio::test]
    async fn forward_event_reports_a_decode_error_for_a_malformed_body() {
        let (bearer, mut rx) = LoopbackBearer::channel();
        let ladder = BearerLadder::new(vec![Box::new(bearer)]);
        // Not valid msgpack for an AtlasEvent.
        match forward_event(&ladder, b"not-an-event", "drone-7").await {
            Forwarded::DecodeError => {}
            _ => panic!("a malformed body must be a decode error"),
        }
        // Nothing was sent on the bearer.
        assert!(rx.try_recv().is_err(), "no event should reach the bearer");
    }

    #[tokio::test]
    async fn forward_event_reports_no_bearer_when_the_ladder_is_empty() {
        let ladder = BearerLadder::new(vec![]);
        let ev = AtlasEvent::new("plugin.atlas.pose", None, vec![9]);
        let body = ev.encode().unwrap();
        match forward_event(&ladder, &body, "drone-7").await {
            Forwarded::Decoded {
                carried: None,
                keyframe: false,
            } => {}
            _ => panic!("an empty ladder carries nothing"),
        }
    }

    /// Just for the panic message above — name the carried kind, if any.
    fn carried_kind(f: &Forwarded) -> Option<BearerKind> {
        match f {
            Forwarded::Decoded { carried, .. } => *carried,
            Forwarded::DecodeError => None,
        }
    }

    #[test]
    fn note_node_change_and_loss_clears_the_bearer() {
        let mut fwd = ForwardStatus::default();
        // First resolve → changed.
        assert!(fwd.note_node(Some("rtx-box".into())));
        // Same node → no change.
        assert!(!fwd.note_node(Some("rtx-box".into())));
        // A carry sets the bearer.
        assert!(fwd.note_carried(Some("direct-lan"), false, 100));
        assert_eq!(fwd.bearer.as_deref(), Some("direct-lan"));
        // Losing the node changes state AND clears the (now meaningless) bearer.
        assert!(fwd.note_node(None));
        assert_eq!(fwd.compute_node_id, None);
        assert_eq!(fwd.bearer, None);
        // An empty advertised id is treated as no node.
        assert!(!fwd.note_node(Some(String::new())));
    }

    #[test]
    fn note_carried_tracks_bearer_change_and_keyframe_time() {
        let mut fwd = ForwardStatus::default();
        // A keyframe carry stamps the time and reports a change.
        assert!(fwd.note_carried(Some("wfb-relay"), true, 1_700));
        assert_eq!(fwd.bearer.as_deref(), Some("wfb-relay"));
        assert_eq!(fwd.last_kf_at_ms, Some(1_700));
        // Same bearer, no keyframe → nothing moved.
        assert!(!fwd.note_carried(Some("wfb-relay"), false, 1_800));
        assert_eq!(fwd.last_kf_at_ms, Some(1_700));
        // A later keyframe advances the time.
        assert!(fwd.note_carried(Some("wfb-relay"), true, 1_900));
        assert_eq!(fwd.last_kf_at_ms, Some(1_900));
        // A declined send (None) keeps the last-known bearer.
        assert!(!fwd.note_carried(None, false, 2_000));
        assert_eq!(fwd.bearer.as_deref(), Some("wfb-relay"));
    }

    #[test]
    fn bearer_label_maps_to_the_gcs_vocabulary() {
        assert_eq!(bearer_label(BearerKind::DirectLan), "direct-lan");
        assert_eq!(bearer_label(BearerKind::WfbRelay), "wfb-relay");
        assert_eq!(bearer_label(BearerKind::Cloud), "cloud");
    }
}
