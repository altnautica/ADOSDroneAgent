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

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;

use ados_atlas_transport::{
    AtlasBearer, AtlasEvent, BearerKind, BearerLadder, LanHttpBearer, WfbRelayBearer,
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

/// Outcome of one forward attempt.
enum Forwarded {
    /// The event decoded; `Some(kind)` carried it, `None` if every bearer
    /// declined / errored (logged; the loop keeps draining the bus).
    Decoded(Option<BearerKind>),
    /// The framed body was not a valid event — logged, no send attempted (does
    /// NOT count as a transport miss, so it never re-resolves the node).
    DecodeError,
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
        // a direct-LAN bearer is currently present.
        let (mut ladder, mut have_lan) = build_ladder(&config, &mut cloud_transport).await;
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
                    match forward_event(&ladder, &body, &config.agent.device_id).await {
                        Forwarded::Decoded(carried) => {
                            if have_lan {
                                if carried == Some(BearerKind::DirectLan) {
                                    lan_misses = 0;
                                } else {
                                    lan_misses += 1;
                                }
                            }
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
                        let (l, lan) = build_ladder(&config, &mut cloud_transport).await;
                        if lan && !have_lan {
                            tracing::info!("atlas forwarder discovered a compute node");
                        }
                        ladder = l;
                        have_lan = lan;
                        lan_misses = 0;
                        last_resolve = Instant::now();
                    }
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
    let mut event = match AtlasEvent::from_msgpack(body) {
        Ok(ev) => ev,
        Err(e) => {
            tracing::warn!(error = %e, "atlas forwarder dropped a malformed event");
            return Forwarded::DecodeError;
        }
    };
    if !device_id.is_empty() {
        event.device_id = Some(device_id.to_string());
    }
    match ladder.send(&event).await {
        Ok(kind) => {
            tracing::debug!(
                topic = %event.topic,
                bearer = ?kind,
                bytes = event.payload.len(),
                "atlas event forwarded"
            );
            Forwarded::Decoded(Some(kind))
        }
        Err(e) => {
            tracing::debug!(
                topic = %event.topic,
                error = %e,
                "atlas event not forwarded (all bearers declined)"
            );
            Forwarded::Decoded(None)
        }
    }
}

/// Resolve the compute node and build the bearer ladder. Returns the ladder and
/// whether a direct-LAN bearer is present (a compute node was resolved). The
/// cloud transport is built at most once (the first time cloud relay is the
/// posture AND the agent is paired) and reused on every rebuild.
async fn build_ladder(
    config: &CloudConfig,
    cloud_transport: &mut Option<Arc<RumqttcTransport>>,
) -> (BearerLadder, bool) {
    let mut bearers: Vec<Box<dyn AtlasBearer>> = Vec::new();

    // ── Direct LAN (first-class): a resolved compute node's job-API URL ──
    let have_lan = match ados_compute::mdns::resolve_compute(RESOLVE_TIMEOUT).await {
        Some((host, port)) => {
            let base = format!("http://{host}:{port}");
            tracing::info!(base = %base, "atlas forwarder resolved compute node");
            bearers.push(Box::new(LanHttpBearer::new(base)));
            true
        }
        None => {
            tracing::debug!("atlas forwarder: no compute node on mDNS yet");
            false
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

    (BearerLadder::new(bearers), have_lan)
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
        let ev = AtlasEvent {
            topic: "atlas.keyframe".into(),
            device_id: None,
            payload: vec![1, 2, 3],
        };
        let body = ev.to_msgpack().unwrap();

        match forward_event(&ladder, &body, "drone-7").await {
            Forwarded::Decoded(Some(BearerKind::Loopback)) => {}
            other => panic!(
                "expected a loopback-carried event, got {:?}",
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
        let ev = AtlasEvent {
            topic: "plugin.atlas.pose".into(),
            device_id: None,
            payload: vec![9],
        };
        let body = ev.to_msgpack().unwrap();
        match forward_event(&ladder, &body, "drone-7").await {
            Forwarded::Decoded(None) => {}
            _ => panic!("an empty ladder carries nothing"),
        }
    }

    /// Just for the panic message above — name the carried kind, if any.
    fn carried_kind(f: &Forwarded) -> Option<BearerKind> {
        match f {
            Forwarded::Decoded(k) => *k,
            Forwarded::DecodeError => None,
        }
    }
}
