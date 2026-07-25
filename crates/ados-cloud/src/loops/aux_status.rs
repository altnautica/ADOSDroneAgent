//! Publish this node's status and identity on the auxiliary radio lane.
//!
//! A node reached only through a ground station cannot be asked anything: the
//! radio link carries no IP, so its HTTP surface is unreachable from the ground
//! side and an operator paired to the ground station sees a nameless row with no
//! board, no services, and no flight-controller state. This loop is the answer,
//! and it is deliberately a PUSH: the drone periodically states what it is,
//! because there is no channel on which it could be asked.
//!
//! Two frames, two cadences. Status goes out every second by default because it
//! changes. Identity goes out every ten because it does not; it exists so the
//! ground station can name its peer and keep naming it after either side
//! restarts.
//!
//! ## Sampling cost is the real constraint
//!
//! Every source here is a file read except one: the service fleet is a
//! `systemctl` fork. At the status cadence that would be a fork per second on a
//! small board, for a fleet that changes on the order of minutes. So the service
//! sample runs on its own slower sub-cadence and is cached between; the frame
//! carries the last real sample rather than a fresh fork per tick. The whole
//! sample runs on the blocking pool, never on the async reactor.
//!
//! ## This lane must never disturb video
//!
//! The producer sends one bounded datagram per tick and never queues. A send
//! that fails is dropped and counted, not retried into a backlog. A refused
//! lane backs off hard rather than reopening every tick, and an operator who
//! disabled the lane is honoured permanently after one log line rather than
//! retried forever. Nothing here can grow without bound or outrun its cadence.
//!
//! ## What a send does and does not prove
//!
//! A successful send means the datagram entered the local kernel buffer. It does
//! not mean the radio radiated it or the ground station decoded it. The only
//! proof of delivery is the receiving side's own counter, which is why the
//! frames carry a sequence number: the ground station can see loss, and neither
//! side infers a working link from the fact that a send returned.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ados_protocol::aux_egress::{AuxEgress, AuxEgressError};
use ados_protocol::aux_mux::AuxChannel;
use ados_protocol::node_status::{NodeIdentity, NodeStatus};
use serde_json::Value;
use tokio::sync::watch;

use crate::config::CloudConfig;
use crate::loops::enrichment::{self, CpuSample};

/// How often the service fleet is re-sampled, regardless of how fast status
/// ticks. The fleet changes rarely and its sample is the one expensive read.
const SERVICE_SAMPLE_INTERVAL: Duration = Duration::from_secs(15);

/// How long the producer waits after the radio refuses to open the lane for a
/// transient reason. Long relative to the status cadence: a lane that is not up
/// is not made to come up faster by asking every second.
const REFUSED_BACKOFF: Duration = Duration::from_secs(30);

/// How often the counters are logged, and only when they have moved.
const REPORT_INTERVAL: Duration = Duration::from_secs(300);

/// The camera-state sidecar, and how stale it may be before it reads as unknown.
const CAMERA_STATE_SIDECAR: &str = "/run/ados/camera-state.json";
const CAMERA_STATE_STALE_S: f64 = 30.0;

/// The video-streams sidecar, and its staleness window (4x the ~5 s re-stamp).
const VIDEO_STREAMS_SIDECAR: &str = "/run/ados/video-streams.json";
const VIDEO_STREAMS_STALE_S: f64 = 20.0;

/// The board sidecar, for the board descriptors.
const BOARD_SIDECAR: &str = "/run/ados/board.json";

/// The radio's auxiliary command socket, under the run dir.
///
/// Named by the wire string rather than by depending on the radio crate: this
/// producer needs one path, not the radio stack, and the MAVLink tee resolves
/// the same socket the same way.
const AUX_CMD_SOCK_FILE: &str = "radio-aux.sock";

/// The run directory, honouring the `ADOS_RUN_DIR` override tests use.
fn run_dir() -> String {
    std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string())
}

/// Cumulative counters for the producer, so a lane that is publishing nothing
/// can be told apart from one that is publishing into a refusal.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ProducerCounters {
    status_sent: u64,
    identity_sent: u64,
    /// Snapshots that had to shed fields to fit. Expected to stay zero; a
    /// non-zero value means the schema has outgrown the frame.
    status_trimmed: u64,
    /// Frames the lane refused for a transient reason.
    refused: u64,
    /// Frames dropped because the send itself failed.
    send_errors: u64,
    /// Frames that could not be encoded at all. Expected to stay zero.
    encode_failures: u64,
}

/// A cached service-fleet sample: counts by state plus the failed unit names.
#[derive(Debug, Default, Clone)]
struct ServiceSummary {
    running: u16,
    failed: u16,
    other: u16,
    failed_names: Vec<String>,
}

/// Reduce the enrichment's `services` array to counts by state plus the names of
/// the failed units.
///
/// The full list is what the heartbeat sends over HTTP; one aux frame cannot
/// carry it and an operator does not need it. What an operator needs is how many
/// are healthy, how many are broken, and WHICH are broken.
///
/// `systemd` reports a stopped unit through several sub-states (`dead`,
/// `exited`, `failed`), and only `failed` is a fault. `exited` in particular is
/// the normal terminal state of a successful one-shot, so folding it into the
/// failed count would report a healthy node as broken.
fn summarize_services(services: &[Value]) -> ServiceSummary {
    let mut out = ServiceSummary::default();
    for svc in services {
        let status = svc.get("status").and_then(Value::as_str).unwrap_or("");
        match status {
            "running" => out.running = out.running.saturating_add(1),
            "failed" => {
                out.failed = out.failed.saturating_add(1);
                if let Some(name) = svc.get("name").and_then(Value::as_str) {
                    out.failed_names.push(name.to_string());
                }
            }
            _ => out.other = out.other.saturating_add(1),
        }
    }
    out
}

/// Read the current camera state, staleness-gated.
///
/// A lingering sidecar from a stopped pipeline must not keep advertising a
/// camera as ready, so an un-refreshed file reads as unknown rather than as its
/// last value (operating rule 44).
fn read_camera_state(path: &str, now: f64) -> Option<String> {
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let updated = doc.get("updated_at_unix").and_then(Value::as_f64)?;
    if updated <= 0.0 || now - updated > CAMERA_STATE_STALE_S {
        return None;
    }
    let state = doc.get("state").and_then(Value::as_str)?;
    matches!(state, "ready" | "missing" | "error").then(|| state.to_string())
}

/// Derive a one-word video state from the streams sidecar, staleness-gated.
///
/// The sidecar carries per-leg liveness, so this reports what is actually
/// happening rather than merely that a pipeline was configured: `streaming` when
/// at least one leg is receiving, `degraded` when every leg that reported
/// liveness is flat, and `idle` when legs exist but none reported. An absent or
/// stale sidecar reads as unknown, never as `stopped` — the producer does not
/// know the pipeline stopped, only that nobody said otherwise.
fn read_video_state(path: &str, now: f64) -> Option<String> {
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let updated = doc.get("updated_at_unix").and_then(Value::as_f64)?;
    if updated <= 0.0 || now - updated > VIDEO_STREAMS_STALE_S {
        return None;
    }
    let streams = doc.get("streams").and_then(Value::as_array)?;
    if streams.is_empty() {
        return None;
    }
    let live: Vec<bool> = streams
        .iter()
        .filter_map(|s| s.get("live").and_then(Value::as_bool))
        .collect();
    Some(
        if live.iter().any(|l| *l) {
            "streaming"
        } else if live.is_empty() {
            "idle"
        } else {
            "degraded"
        }
        .to_string(),
    )
}

/// Read the board descriptors from the board sidecar: name, SoC, tier.
fn read_board(path: &str) -> (Option<String>, Option<String>, Option<u8>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (None, None, None);
    };
    let Ok(doc) = serde_json::from_str::<Value>(&text) else {
        return (None, None, None);
    };
    let s = |k: &str| {
        doc.get(k)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    let tier = doc
        .get("tier")
        .and_then(Value::as_u64)
        .and_then(|t| u8::try_from(t).ok());
    (s("name"), s("soc"), tier)
}

/// Everything one tick reads from the node, gathered on the blocking pool.
struct Sample {
    enrichment: Value,
    camera: Option<String>,
    video: Option<String>,
    board: (Option<String>, Option<String>, Option<u8>),
}

/// Gather one sample. Blocking by nature (reads `/proc`, the state socket, three
/// sidecars, and optionally forks `systemctl`), so callers run it off the
/// reactor.
fn gather(prev_cpu: &mut Option<CpuSample>, with_services: bool, now: f64) -> Sample {
    Sample {
        enrichment: enrichment::build_native_enrichment_with(prev_cpu, with_services),
        camera: read_camera_state(CAMERA_STATE_SIDECAR, now),
        video: read_video_state(VIDEO_STREAMS_SIDECAR, now),
        board: read_board(BOARD_SIDECAR),
    }
}

/// Project one sample plus the cached service summary into the compact snapshot.
///
/// Pure, so the whole projection is testable without a socket, a sidecar, or a
/// radio. Every field is read as an option: a source that failed leaves its
/// field absent rather than asserting a fabricated value.
fn project(
    device_id: &str,
    seq: u32,
    uptime_s: u32,
    version: &str,
    sample: &Sample,
    services: Option<&ServiceSummary>,
) -> NodeStatus {
    let e = &sample.enrichment;
    let f32_of = |k: &str| e.get(k).and_then(Value::as_f64).map(|v| v as f32);
    let str_of = |k: &str| e.get(k).and_then(Value::as_str);
    let bool_of = |k: &str| e.get(k).and_then(Value::as_bool);

    let (board_name, board_soc, board_tier) = &sample.board;
    let mut status = NodeStatus::new(device_id, seq)
        .with_agent(Some(uptime_s), Some(version))
        .with_board(board_name.as_deref(), board_soc.as_deref(), *board_tier)
        .with_fc(
            bool_of("fcConnected"),
            bool_of("mavlinkAlive"),
            str_of("fcVariant"),
            str_of("fcFirmware"),
        )
        .with_resources(
            f32_of("cpuPercent"),
            f32_of("memoryPercent"),
            f32_of("diskPercent"),
            f32_of("temperature"),
        )
        .with_payload(sample.camera.as_deref(), sample.video.as_deref());

    if let Some(s) = services {
        status = status.with_services(s.running, s.failed, s.other, &s.failed_names);
    }
    status
}

fn now_unix() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Publish status and identity on the auxiliary lane until shutdown.
///
/// Returns early, having done nothing, when the node has no device id (nothing
/// to identify) or the operator turned the producer off.
pub async fn run(config: Arc<CloudConfig>, mut shutdown: watch::Receiver<bool>) {
    let wfb = &config.video.wfb;
    if !wfb.aux_status_enabled {
        tracing::info!("aux_status_producer_disabled");
        return;
    }
    let device_id = config.agent.device_id.clone();
    if device_id.trim().is_empty() {
        // Without an id the frames correlate to nothing on the far side, so
        // publishing them would only add traffic.
        tracing::warn!("aux_status_producer_no_device_id");
        return;
    }

    let status_interval = wfb.status_interval();
    let identity_interval = wfb.identity_interval();
    let egress = AuxEgress::new(format!("{}/{AUX_CMD_SOCK_FILE}", run_dir()));
    let version = env!("CARGO_PKG_VERSION").to_string();
    let identity = NodeIdentity::build(
        &device_id,
        Some(&config.agent.name),
        Some(&config.agent.profile),
        Some(&version),
    );

    tracing::info!(
        device_id = %device_id,
        status_interval_s = status_interval.as_secs_f64(),
        identity_interval_s = identity_interval.as_secs_f64(),
        "aux_status_producer_started"
    );

    let started = Instant::now();
    let mut tick = tokio::time::interval(status_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut prev_cpu: Option<CpuSample> = None;
    let mut services: Option<ServiceSummary> = None;
    let mut last_service_sample: Option<Instant> = None;
    let mut last_identity: Option<Instant> = None;
    let mut counters = ProducerCounters::default();
    let mut last_report = ProducerCounters::default();
    let mut last_report_at = Instant::now();
    let mut seq: u32 = 0;
    // Set once the operator's dead-switch refuses the lane. The refusal is a
    // deliberate choice, not a fault, so it is honoured for the life of the
    // process after one log line instead of being retried every tick.
    let mut disabled = false;
    let mut backoff_until: Option<Instant> = None;

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { break; }
            }
            _ = tick.tick() => {
                if disabled {
                    continue;
                }
                let now = Instant::now();
                if backoff_until.is_some_and(|until| now < until) {
                    continue;
                }

                let want_services = last_service_sample
                    .is_none_or(|at| now.duration_since(at) >= SERVICE_SAMPLE_INTERVAL);

                let wall = now_unix();
                let sample = match tokio::task::spawn_blocking(move || {
                    let mut cpu = prev_cpu;
                    let s = gather(&mut cpu, want_services, wall);
                    (s, cpu)
                })
                .await
                {
                    Ok((sample, cpu)) => {
                        prev_cpu = cpu;
                        sample
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "aux_status_sample_failed");
                        continue;
                    }
                };

                // Refresh the cached fleet only when this tick actually sampled
                // it. A tick that skipped the fork keeps the previous summary
                // rather than dropping the field.
                if want_services {
                    if let Some(list) = sample.enrichment.get("services").and_then(Value::as_array) {
                        services = Some(summarize_services(list));
                    }
                    last_service_sample = Some(now);
                }

                seq = seq.wrapping_add(1);
                let snapshot = project(
                    &device_id,
                    seq,
                    started.elapsed().as_secs().min(u32::MAX as u64) as u32,
                    &version,
                    &sample,
                    services.as_ref(),
                );

                match snapshot.encode() {
                    Some((bytes, trimmed)) => {
                        if trimmed > 0 {
                            counters.status_trimmed += 1;
                            tracing::debug!(steps = trimmed, "aux_status_snapshot_trimmed");
                        }
                        match egress.send(AuxChannel::Status, &bytes).await {
                            Ok(()) => counters.status_sent += 1,
                            Err(e) => {
                                record_send_error(&e, &mut counters, &mut disabled, &mut backoff_until, now);
                            }
                        }
                    }
                    None => {
                        counters.encode_failures += 1;
                        tracing::warn!("aux_status_snapshot_unencodable");
                    }
                }

                // Identity, on its own slower schedule and only once the lane is
                // known to work: no point opening it just to say hello.
                if !disabled
                    && last_identity.is_none_or(|at| now.duration_since(at) >= identity_interval)
                {
                    if let Some(bytes) = identity.encode() {
                        match egress.send(AuxChannel::Identity, &bytes).await {
                            Ok(()) => {
                                counters.identity_sent += 1;
                                last_identity = Some(now);
                            }
                            Err(e) => {
                                record_send_error(&e, &mut counters, &mut disabled, &mut backoff_until, now);
                            }
                        }
                    }
                }

                if now.duration_since(last_report_at) >= REPORT_INTERVAL {
                    last_report_at = now;
                    if counters != last_report {
                        tracing::info!(
                            status_sent = counters.status_sent,
                            identity_sent = counters.identity_sent,
                            status_trimmed = counters.status_trimmed,
                            refused = counters.refused,
                            send_errors = counters.send_errors,
                            encode_failures = counters.encode_failures,
                            "aux_status_producer_counters"
                        );
                        last_report = counters;
                    }
                }
            }
        }
    }
    tracing::info!("aux_status_producer_stopped");
}

/// Classify a failed send: an operator-disabled lane stops the producer for good
/// after one line, anything else backs off and is counted.
fn record_send_error(
    error: &AuxEgressError,
    counters: &mut ProducerCounters,
    disabled: &mut bool,
    backoff_until: &mut Option<Instant>,
    now: Instant,
) {
    match error {
        AuxEgressError::Disabled => {
            *disabled = true;
            tracing::info!("aux_status_producer_lane_disabled_by_operator");
        }
        AuxEgressError::Refused(e) => {
            counters.refused += 1;
            *backoff_until = Some(now + REFUSED_BACKOFF);
            tracing::debug!(error = %e, "aux_status_lane_refused");
        }
        other => {
            counters.send_errors += 1;
            *backoff_until = Some(now + REFUSED_BACKOFF);
            tracing::debug!(error = %other, "aux_status_send_failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_with(enrichment: Value) -> Sample {
        Sample {
            enrichment,
            camera: Some("ready".to_string()),
            video: Some("streaming".to_string()),
            board: (
                Some("rock-5c-lite".to_string()),
                Some("rk3588s2".to_string()),
                Some(3),
            ),
        }
    }

    #[test]
    fn exited_units_are_not_counted_as_failures() {
        // The trap: `exited` is the normal terminal state of a successful
        // one-shot. Folding it into the failed count would report a healthy node
        // as broken, which is exactly the false alarm this surface must not
        // raise.
        let services = vec![
            json!({"name": "ados-control", "status": "running"}),
            json!({"name": "ados-video", "status": "running"}),
            json!({"name": "ados-macpin", "status": "exited"}),
            json!({"name": "ados-net", "status": "dead"}),
            json!({"name": "ados-vision", "status": "failed"}),
        ];
        let s = summarize_services(&services);
        assert_eq!(s.running, 2);
        assert_eq!(s.failed, 1);
        assert_eq!(s.other, 2);
        assert_eq!(s.failed_names, vec!["ados-vision".to_string()]);
    }

    #[test]
    fn a_projected_snapshot_carries_the_live_fields_and_fits() {
        let sample = sample_with(json!({
            "cpuPercent": 12.5,
            "memoryPercent": 48.0,
            "diskPercent": 31.25,
            "temperature": 52.0,
            "fcConnected": true,
            "mavlinkAlive": false,
            "fcVariant": "betaflight",
            "fcFirmware": "betaflight",
        }));
        let services = ServiceSummary {
            running: 14,
            failed: 1,
            other: 3,
            failed_names: vec!["ados-vision".to_string()],
        };
        let s = project("abcdef123456", 7, 3600, "1.2.3", &sample, Some(&services));

        assert_eq!(s.id, "abcdef123456");
        assert_eq!(s.sq, 7);
        assert_eq!(s.fc, Some(true));
        assert_eq!(s.fa, Some(false));
        assert_eq!(s.cp, Some(12.5));
        assert_eq!(s.sr, Some(14));
        assert_eq!(s.sf, Some(1));
        assert_eq!(s.bn.as_deref(), Some("rock-5c-lite"));
        assert_eq!(s.cs.as_deref(), Some("ready"));
        assert_eq!(s.vs.as_deref(), Some("streaming"));

        let (bytes, trimmed) = s.encode().expect("encodes");
        assert_eq!(trimmed, 0, "a real snapshot must not need trimming");
        assert!(bytes.len() < 400, "snapshot is {} bytes", bytes.len());
    }

    #[test]
    fn a_failed_source_leaves_its_field_absent_not_zero() {
        // The honesty invariant: an empty enrichment (every source failed) must
        // produce a snapshot that says nothing, not one claiming 0% CPU and a
        // disconnected flight controller.
        let sample = Sample {
            enrichment: json!({}),
            camera: None,
            video: None,
            board: (None, None, None),
        };
        let s = project("abcdef123456", 1, 10, "1.2.3", &sample, None);
        assert_eq!(s.cp, None);
        assert_eq!(s.fc, None);
        assert_eq!(s.fa, None);
        assert_eq!(s.sr, None);
        assert_eq!(s.bn, None);
        assert_eq!(s.cs, None);
        // The core is still present, so the frame remains useful as a liveness
        // and identity signal even when every sensor read failed.
        assert_eq!(s.id, "abcdef123456");
        assert_eq!(s.sq, 1);
    }

    #[test]
    fn camera_and_video_states_are_staleness_gated() {
        let dir = tempfile::tempdir().unwrap();
        let cam = dir.path().join("camera-state.json");
        let vid = dir.path().join("video-streams.json");
        let now = 1_700_000_000.0;

        std::fs::write(
            &cam,
            json!({"state": "ready", "updated_at_unix": now}).to_string(),
        )
        .unwrap();
        std::fs::write(
            &vid,
            json!({"updated_at_unix": now, "streams": [{"id": "main", "live": true}]}).to_string(),
        )
        .unwrap();
        assert_eq!(
            read_camera_state(cam.to_str().unwrap(), now),
            Some("ready".into())
        );
        assert_eq!(
            read_video_state(vid.to_str().unwrap(), now),
            Some("streaming".into())
        );

        // A lingering sidecar from a stopped pipeline must read as unknown, not
        // keep advertising its last value.
        let later = now + 120.0;
        assert_eq!(read_camera_state(cam.to_str().unwrap(), later), None);
        assert_eq!(read_video_state(vid.to_str().unwrap(), later), None);
    }

    #[test]
    fn video_state_distinguishes_flat_legs_from_unreported_ones() {
        let dir = tempfile::tempdir().unwrap();
        let vid = dir.path().join("video-streams.json");
        let now = 1_700_000_000.0;
        let write = |streams: Value| {
            std::fs::write(
                &vid,
                json!({"updated_at_unix": now, "streams": streams}).to_string(),
            )
            .unwrap();
        };

        // Every reporting leg flat is a real degradation.
        write(json!([{"id": "main", "live": false}]));
        assert_eq!(
            read_video_state(vid.to_str().unwrap(), now),
            Some("degraded".into())
        );
        // No leg reported liveness at all: idle, which is not the same claim.
        write(json!([{"id": "main"}]));
        assert_eq!(
            read_video_state(vid.to_str().unwrap(), now),
            Some("idle".into())
        );
        // No legs at all: unknown, not a fabricated state.
        write(json!([]));
        assert_eq!(read_video_state(vid.to_str().unwrap(), now), None);
    }

    #[test]
    fn a_missing_or_malformed_sidecar_reads_unknown() {
        assert_eq!(read_camera_state("/nonexistent/camera.json", 1.0), None);
        assert_eq!(read_video_state("/nonexistent/video.json", 1.0), None);
        assert_eq!(read_board("/nonexistent/board.json"), (None, None, None));
    }

    #[test]
    fn an_operator_disabled_lane_stops_the_producer_but_a_refusal_backs_off() {
        // A deliberate operator choice and a transient refusal must not be
        // treated alike: the first is honoured for good, the second retried.
        let mut counters = ProducerCounters::default();
        let mut disabled = false;
        let mut backoff = None;
        let now = Instant::now();

        record_send_error(
            &AuxEgressError::Refused("no radio".into()),
            &mut counters,
            &mut disabled,
            &mut backoff,
            now,
        );
        assert!(
            !disabled,
            "a transient refusal must not disable the producer"
        );
        assert_eq!(counters.refused, 1);
        assert!(backoff.is_some(), "a refusal must back off");

        record_send_error(
            &AuxEgressError::Disabled,
            &mut counters,
            &mut disabled,
            &mut backoff,
            now,
        );
        assert!(disabled, "an operator-disabled lane must stop the producer");
    }

    #[test]
    fn the_configured_cadence_can_never_become_a_busy_loop() {
        use crate::config::{WfbSection, AUX_MIN_INTERVAL_S};
        // A zero or negative interval in config must be floored, not obeyed: the
        // lane is shared with video and a spinning producer would flood it.
        let mut w = WfbSection {
            aux_status_interval_s: 0.0,
            aux_identity_interval_s: -5.0,
            ..Default::default()
        };
        assert_eq!(w.status_interval().as_secs_f64(), AUX_MIN_INTERVAL_S);
        assert_eq!(w.identity_interval().as_secs_f64(), AUX_MIN_INTERVAL_S);

        // A sane value is respected.
        w.aux_status_interval_s = 2.5;
        assert_eq!(w.status_interval().as_secs_f64(), 2.5);
    }

    #[test]
    fn defaults_are_a_one_second_status_and_ten_second_identity() {
        let w = crate::config::WfbSection::default();
        assert!(w.aux_status_enabled);
        assert_eq!(w.status_interval(), Duration::from_secs(1));
        assert_eq!(w.identity_interval(), Duration::from_secs(10));
    }
}
