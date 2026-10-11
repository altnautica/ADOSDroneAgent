//! The long-running FC-side tasks: the link itself, routing, signing,
//! cadence reporting, the heartbeat and the stream/param ticks.

use std::sync::Arc;
use std::time::Duration;

use ados_protocol::shutdown::Shutdown;

use super::startup::{demo_enabled, CADENCE_REPORT_INTERVAL};
use super::Tasks;
use crate::config::MavlinkConfig;
use crate::connection::FcConnection;
use crate::{routing, signing};

/// Spawn the FC link and its periodic companions.
pub(super) fn spawn_core(
    cfg: &MavlinkConfig,
    fc: &Arc<FcConnection>,
    cancel: &Shutdown,
    dir: &str,
    tasks: &mut Tasks,
) {
    // FC connect + read loop. In demo mode a synthetic source feeds the same
    // fan-out, state, and proxy paths a serial FC would; the serial path is
    // untouched when demo mode is off (the default).
    let demo = demo_enabled();
    {
        let fc = fc.clone();
        let cancel = cancel.clone();
        if demo {
            tracing::info!("mavlink_router_demo_mode");
            tasks.push(tokio::spawn(async move { fc.run_demo(cancel).await }));
        } else {
            tasks.push(tokio::spawn(async move { fc.run(cancel).await }));
        }
    }

    // Route-aware forwarding: one classifier decodes each FC frame once and
    // picks its audience, and the route table is published every 2 s when it
    // changed. Off (the default), neither runs and every consumer reads the
    // flat fan-out.
    if fc.routes().enabled() {
        tracing::info!("mavlink_route_aware_enabled");
        let hub = fc.routes().clone();
        let frames = fc.subscribe();
        let c = cancel.clone();
        tasks.push(tokio::spawn(routing::run_classifier(hub, frames, c)));
        let hub = fc.routes().clone();
        let path = std::path::Path::new(dir).join(routing::SIDECAR_NAME);
        let c = cancel.clone();
        tasks.push(tokio::spawn(routing::run_sidecar(hub, path, c)));
    }

    // Agent-held signing: load the key before the first heartbeat, then
    // re-read it and persist the signing timestamp every 10 s and at shutdown,
    // so an enrolment or a disable reaches this process without a restart.
    fc.agent_signing().refresh(std::time::SystemTime::now());
    tracing::info!(
        agent_signing = fc.agent_signing().state(),
        "mavlink_agent_signing"
    );
    if cfg.agent_signing {
        let fc = fc.clone();
        let cancel = cancel.clone();
        tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(signing::PERSIST_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        fc.agent_signing().persist();
                        fc.agent_signing().refresh(std::time::SystemTime::now());
                    }
                    _ = cancel.wait() => {
                        fc.agent_signing().persist();
                        break;
                    }
                }
            }
        }));
    }

    // Ship the attitude-cadence distribution to the logging store.
    //
    // The measurement lives in memory in the read loop, which answers the
    // question only for whoever is holding the process open at the time. A
    // decision about closing a control loop over this link needs the tail
    // across a whole bench run, and needs it to survive the run -- so the
    // percentiles go where they can be queried afterwards rather than only
    // observed live.
    //
    // Every ten seconds, not every arrival: the samples are already summarised
    // by then, and emitting per-message would put a few hundred writes a second
    // onto a store this measurement is not supposed to disturb.
    {
        let fc = fc.clone();
        let cancel = cancel.clone();
        let metrics = ados_protocol::logd::emitter::IngestEmitter::new("ados-mavlink");
        tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(CADENCE_REPORT_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        let s = fc.attitude_cadence().await;
                        // Nothing to say before the first gap exists. An empty
                        // window emitting zeros would put a fabricated 0 Hz into
                        // the series and make a link that has not started look
                        // like one that stopped.
                        let (Some(hz), Some(p50), Some(p95), Some(p99)) =
                            (s.achieved_hz, s.p50, s.p95, s.p99)
                        else {
                            continue;
                        };
                        use ados_protocol::logd::{Fields, Value};
                        let mut tags = Fields::new();
                        tags.insert("message".to_string(), Value::from("attitude"));
                        let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
                        metrics.emit_metric("fc.attitude.achieved_hz", hz, tags.clone());
                        metrics.emit_metric("fc.attitude.period_p50_ms", ms(p50), tags.clone());
                        metrics.emit_metric("fc.attitude.period_p95_ms", ms(p95), tags.clone());
                        metrics.emit_metric("fc.attitude.period_p99_ms", ms(p99), tags.clone());
                        metrics.emit_metric(
                            "fc.attitude.deadline_misses",
                            s.missed_deadline as f64,
                            tags,
                        );
                    }
                    _ = cancel.wait() => break,
                }
            }
        }));
    }

    // 1 Hz companion heartbeat, plus the TX-liveness watchdog that judges it.
    //
    // Deliberately the same task and the same tick: the heartbeat is the demand
    // the watchdog measures against. A writer that exists and a future that has
    // not returned prove nothing about whether bytes are leaving; the heartbeat
    // guarantees the TX byte counter advances at least once a second on any live
    // link, so a flat counter is a dead writer rather than a quiet one. Ordered
    // heartbeat-then-watchdog so the watchdog reads a counter this tick already
    // had its chance to move.
    {
        let fc = fc.clone();
        let cancel = cancel.clone();
        tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        fc.send_heartbeat().await;
                        fc.tick_tx_watchdog().await;
                    }
                    _ = cancel.wait() => break,
                }
            }
        }));
    }

    // Adaptive stream cadence + parameter sweep (evaluated every second).
    {
        let fc = fc.clone();
        let cancel = cancel.clone();
        tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        fc.tick_streams().await;
                        fc.tick_banner_request().await;
                        fc.tick_param_sweep().await;
                    }
                    _ = cancel.wait() => break,
                }
            }
        }));
    }
}

/// The PIC sidecar and pairing feed the injector gate and the swarm loop
/// read, so neither ever reads those files on a command path.
pub(super) fn spawn_gate_feed(fc: &Arc<FcConnection>, cancel: &Shutdown, tasks: &mut Tasks) {
    {
        let fc = fc.clone();
        let cancel = cancel.clone();
        tasks.push(tokio::spawn(async move { fc.run_gate_feed(cancel).await }));
    }
    tracing::info!(
        injector_arbitration = fc.injector_arbitration_state(),
        "mavlink_injector_arbitration"
    );
}
