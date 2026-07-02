//! The sidecar tailer.
//!
//! Several agent services publish a small JSON snapshot file under the runtime
//! directory and refresh it on a cadence. The tailer watches each of those files
//! and turns their freshness and contents into durable rows on the ingest
//! channel:
//!
//! - A file older than its freshness budget emits a `sidecar.stale` event (the
//!   producer stalled). The event fires once per stale episode, not on every
//!   poll, so a long stall is one row, not a storm.
//! - A file that was present and then disappears emits a `sidecar.drop` event
//!   (the producer died or unmounted).
//! - A fresh file's meaningful numeric scalars are sampled into [`TelemetryFrame`]
//!   rows so radio and video history is durable and time-aligned with the logs.
//!
//! The poll step and the loop are split for testability: [`poll_once`] is pure
//! over an injectable root directory and the per-file state, so a test lays down
//! fixture files in a tempdir, mutates their mtime/content, and asserts the exact
//! events. [`run_sidecar_tailer`] wraps it with the daemon's poll timer and
//! shutdown. It only ever reads the files.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tokio::sync::mpsc;

use ados_protocol::logd::{EventFrame, IngestFrame, Level, TelemetryFrame};

use super::{Shutdown, SOURCE_SIDECAR};

/// How often the directory is polled.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Default freshness budget. A sidecar not refreshed within this window is
/// stale. The slowest documented writer cadence is ~5 s, so a 10 s budget is a
/// 2x margin that avoids false positives from one missed refresh.
const DEFAULT_STALENESS: Duration = Duration::from_secs(10);

/// Keep-alive cadence for the string-carrying snapshot events. They normally fire
/// only on a transition, so under a long steady state the row ages out of the
/// store query window while the numeric series stays fresh — a store-first read
/// would then see a fresh metric but a missing string event. Re-emitting the
/// snapshot at least this often keeps the latest string event inside the window
/// without storming the store. Comfortably under the shortest query window the
/// route uses.
const STRING_EVENT_KEEPALIVE: Duration = Duration::from_secs(60);

/// One numeric scalar to sample out of a sidecar: the dotted JSON path and the
/// metric key it is recorded under.
struct ScalarMap {
    path: &'static str,
    key: &'static str,
}

/// The string-carrying snapshot a sidecar can emit as an event.
///
/// The scalar path samples only numbers, so a sidecar whose route returns string
/// fields (e.g. the air pipeline's `pipeline_state` / `encoder_name`, or the SEI
/// tap's `source`) needs a second carrier. This describes that carrier: an event
/// of `kind` whose detail map carries the named string `fields` verbatim. The
/// event fires on every fresh poll where the keyed string field is present and
/// changed since the last emission, so the latest event always reflects the
/// current snapshot while a steady state does not storm the store.
struct StringEventSpec {
    /// The event `kind` the carried strings ride under.
    kind: &'static str,
    /// The string JSON paths copied into the event detail (key == path tail).
    fields: &'static [&'static str],
    /// The field whose value gates a re-emission (the snapshot's identity). An
    /// event fires only when this field's value changes between polls.
    transition_on: &'static str,
}

/// A sidecar the tailer watches: its file name, its freshness budget, the source
/// tag stamped on the metrics it yields, and the scalars sampled from it.
struct SidecarSpec {
    /// File name under the runtime root.
    name: &'static str,
    /// Freshness budget before a `sidecar.stale` event fires.
    staleness: Duration,
    /// The `source` tag stamped on this sidecar's sampled metrics.
    source: &'static str,
    /// Numeric scalars sampled into metrics.
    scalars: &'static [ScalarMap],
    /// The string-carrying snapshot event, when the sidecar has string fields a
    /// route needs back. `None` for number-only sidecars.
    string_event: Option<StringEventSpec>,
}

/// The watched sidecars, their freshness budgets, and the scalars sampled from
/// each. The file names match the agent's canonical runtime sidecar paths.
const SPECS: &[SidecarSpec] = &[
    SidecarSpec {
        name: "wfb-stats.json",
        staleness: DEFAULT_STALENESS,
        source: "wfb",
        scalars: &[
            ScalarMap {
                path: "rssi",
                key: "link.rssi.dbm",
            },
            ScalarMap {
                path: "snr",
                key: "link.snr.db",
            },
            ScalarMap {
                path: "fec_loss",
                key: "link.fec.loss_rate",
            },
            ScalarMap {
                path: "bitrate_mbps",
                key: "link.bitrate.mbps",
            },
        ],
        string_event: None,
    },
    // The air-side pipeline snapshot (`AirPipelineStats.to_dict()` + a wall-clock
    // `updated_at_ms`). The numeric fields are sampled into a `video.air.*` metric
    // series; the three string fields (`camera_source`, `encoder_name`,
    // `pipeline_state`) ride a `video.air_state` event so they round-trip back to
    // the air-pipeline route. The monotonic-clock floats (`started_at`,
    // `last_state_change_at`, `last_buffer_at`) carry no cross-process meaning and
    // are not sampled; the route serves those live.
    SidecarSpec {
        name: "air-pipeline.json",
        staleness: DEFAULT_STALENESS,
        source: "video",
        scalars: &[
            ScalarMap {
                path: "encoder_fps",
                key: "video.air.encoder_fps",
            },
            ScalarMap {
                path: "encoded_kbps",
                key: "video.air.encoded_kbps",
            },
            ScalarMap {
                path: "sei_injected_count",
                key: "video.air.sei_injected_count",
            },
            ScalarMap {
                path: "udp_bytes_out",
                key: "video.air.udp_bytes_out",
            },
            ScalarMap {
                path: "restart_count",
                key: "video.air.restart_count",
            },
            ScalarMap {
                path: "tx_silent_kicks",
                key: "video.air.tx_silent_kicks",
            },
            ScalarMap {
                path: "bus_errors",
                key: "video.air.bus_errors",
            },
            ScalarMap {
                path: "updated_at_ms",
                key: "video.air.updated_at_ms",
            },
            ScalarMap {
                path: "encoder_hw_accel",
                key: "video.air.encoder_hw_accel",
            },
            ScalarMap {
                path: "cloud_branch_open",
                key: "video.air.cloud_branch_open",
            },
        ],
        string_event: Some(StringEventSpec {
            kind: "video.air_state",
            fields: &["camera_source", "encoder_name", "pipeline_state"],
            transition_on: "pipeline_state",
        }),
    },
    // The SEI glass-to-glass latency snapshot written by the drone-side tap when
    // SEI latency is enabled. The numeric fields ride a `video.latency.*` series;
    // `source` rides a `video.latency_source` event. The display service writes an
    // unrelated framebuffer-stats blob to the same file on an LCD node — that blob
    // has none of these keys, so the number lookups simply miss and nothing wrong
    // is recorded.
    SidecarSpec {
        name: "lcd-latency.json",
        staleness: DEFAULT_STALENESS,
        source: "video",
        scalars: &[
            ScalarMap {
                path: "latency_ms",
                key: "video.latency.glass_ms",
            },
            ScalarMap {
                path: "latency_ewma_ms",
                key: "video.latency.ewma_ms",
            },
            ScalarMap {
                path: "pipeline_latency_ms",
                key: "video.latency.pipeline_ms",
            },
            ScalarMap {
                path: "samples",
                key: "video.latency.samples",
            },
        ],
        string_event: Some(StringEventSpec {
            kind: "video.latency_source",
            fields: &["source"],
            transition_on: "source",
        }),
    },
    SidecarSpec {
        name: "health.json",
        staleness: DEFAULT_STALENESS,
        source: "health",
        scalars: &[
            ScalarMap {
                path: "cpu_load_1m",
                key: "cpu.load_avg_1m",
            },
            ScalarMap {
                path: "memory_used_pct",
                key: "memory.used_pct",
            },
            ScalarMap {
                path: "disk_used_pct",
                key: "disk.used_pct",
            },
        ],
        string_event: None,
    },
    SidecarSpec {
        name: "camera-state.json",
        staleness: DEFAULT_STALENESS,
        source: "camera",
        scalars: &[
            ScalarMap {
                path: "width",
                key: "camera.resolution.w",
            },
            ScalarMap {
                path: "height",
                key: "camera.resolution.h",
            },
            ScalarMap {
                path: "fps",
                key: "camera.fps",
            },
        ],
        string_event: None,
    },
    SidecarSpec {
        name: "hop-supervisor.json",
        staleness: DEFAULT_STALENESS,
        source: "hop",
        scalars: &[],
        string_event: None,
    },
    SidecarSpec {
        name: "peer-presence.json",
        staleness: DEFAULT_STALENESS,
        source: "peer",
        scalars: &[],
        string_event: None,
    },
    SidecarSpec {
        name: "mesh-state.json",
        staleness: DEFAULT_STALENESS,
        source: "mesh",
        scalars: &[],
        string_event: None,
    },
];

/// Per-file tracking carried across polls: whether the file was present last
/// time, whether a stale event has already fired for the current stale episode
/// (so a long stall is one event, not one per poll), the last value of the
/// string-event transition field (so the event fires once per change, not on
/// every fresh poll of a steady snapshot), and when the string event last fired
/// (so a steady snapshot re-emits on the keep-alive cadence rather than aging out
/// of the query window).
#[derive(Debug, Default, Clone)]
struct FileState {
    present: bool,
    stale_reported: bool,
    last_transition: Option<String>,
    last_string_event_us: Option<i64>,
}

/// The tailer's cross-poll state: one [`FileState`] per watched sidecar, keyed by
/// file name.
#[derive(Debug, Default)]
struct TailerState {
    files: BTreeMap<&'static str, FileState>,
}

impl TailerState {
    fn entry(&mut self, name: &'static str) -> &mut FileState {
        self.files.entry(name).or_default()
    }
}

/// Run the sidecar tailer until `shutdown` resolves, polling `root` every
/// [`POLL_INTERVAL`]. Each poll runs the file stats/reads on a blocking thread so
/// the runtime is never blocked on file IO, then sends the derived frames.
///
/// `root` is the injectable runtime directory: the agent runtime dir in
/// production, a fixture tree in a test.
pub async fn run_sidecar_tailer(
    root: impl Into<PathBuf>,
    tx: mpsc::Sender<IngestFrame>,
    mut shutdown: Shutdown,
) {
    let root = root.into();
    let mut state = TailerState::default();
    tracing::info!(root = %root.display(), "sidecar tailer started");
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = shutdown.recv() => {
                tracing::info!("sidecar tailer stopping");
                return;
            }
            _ = ticker.tick() => {
                // The stats/reads are synchronous; run them off the runtime and
                // move the state in and back out so it persists across polls.
                let root_for_pass = root.clone();
                let now = now_us_systemtime();
                let (frames, moved) = match tokio::task::spawn_blocking(move || {
                    let frames = poll_once(&root_for_pass, &mut state, now);
                    (frames, state)
                })
                .await
                {
                    Ok(pair) => pair,
                    Err(e) => {
                        tracing::warn!(error = %e, "sidecar poll pass failed to join");
                        return;
                    }
                };
                state = moved;
                for frame in frames {
                    if tx.send(frame).await.is_err() {
                        return; // writer side gone
                    }
                }
            }
        }
    }
}

/// Run one poll over `root`, updating `state` and returning the frames it
/// derives. `now_us` is the current microsecond-epoch time, passed in so a test
/// can drive deterministic staleness against a fixture's mtime.
///
/// For each watched sidecar:
/// - absent now but present before -> one `sidecar.drop` event;
/// - present but older than its budget -> one `sidecar.stale` event per episode;
/// - present and fresh -> reset the stale flag and sample its scalars.
fn poll_once(root: &Path, state: &mut TailerState, now_us: i64) -> Vec<IngestFrame> {
    let mut out: Vec<IngestFrame> = Vec::new();
    for spec in SPECS {
        let path = root.join(spec.name);
        let mtime_us = file_mtime_us(&path);
        let st = state.entry(spec.name);
        match mtime_us {
            None => {
                // Absent. If it was present before, the producer dropped it.
                if st.present {
                    out.push(IngestFrame::Event(drop_event(spec.name, now_us)));
                }
                st.present = false;
                st.stale_reported = false;
                // A producer that dropped the file restarts its snapshot identity:
                // forget the last transition and the last emit time so the next
                // fresh value re-emits immediately.
                st.last_transition = None;
                st.last_string_event_us = None;
            }
            Some(mtime) => {
                let age_us = now_us.saturating_sub(mtime);
                let stale = age_us > spec.staleness.as_micros() as i64;
                if stale {
                    if !st.stale_reported {
                        out.push(IngestFrame::Event(stale_event(spec.name, age_us, now_us)));
                        st.stale_reported = true;
                    }
                    // A stale file's contents are not sampled (the writer
                    // stalled, so the values are themselves stale).
                } else {
                    // Fresh: reset the episode, sample the scalars, and emit the
                    // string-carrying snapshot event when its identity changed OR
                    // when the keep-alive interval has elapsed since the last emit
                    // (so a steady snapshot does not age out of the query window).
                    st.stale_reported = false;
                    let last = st.last_transition.take();
                    let last_emit = st.last_string_event_us;
                    let emitted =
                        sample_fresh(&path, spec, now_us, last.as_deref(), last_emit, &mut out);
                    match emitted {
                        Some((value, ts)) => {
                            st.last_transition = Some(value);
                            st.last_string_event_us = Some(ts);
                        }
                        None => {
                            // No emit this poll: keep the prior transition value and
                            // the prior emit timestamp for the next keep-alive check.
                            st.last_transition = last;
                        }
                    }
                }
                st.present = true;
            }
        }
    }
    out
}

/// Read a fresh sidecar's JSON once and append (1) one telemetry frame per
/// present numeric scalar, and (2) the string-carrying snapshot event when its
/// transition field changed since `last_transition` OR the keep-alive interval
/// has elapsed since `last_string_event_us`. A missing or malformed file is
/// skipped without an error: the freshness check already passed, so a parse
/// failure is a transient write race, not a fault.
///
/// Returns `(value, now_us)` when an event was emitted, so the caller can carry
/// the transition value and the emit time forward; `None` means nothing was
/// emitted (the caller keeps the prior value + emit time).
fn sample_fresh(
    path: &Path,
    spec: &SidecarSpec,
    now_us: i64,
    last_transition: Option<&str>,
    last_string_event_us: Option<i64>,
    out: &mut Vec<IngestFrame>,
) -> Option<(String, i64)> {
    if spec.scalars.is_empty() && spec.string_event.is_none() {
        return None;
    }
    let body = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return None,
    };
    let json: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return None,
    };
    // Best-effort schema-drift signal (never reject): if this sidecar is
    // registered, warn when its on-disk `version` differs from the version this
    // build expects, then sample anyway. Keyed on the shared registry (the file
    // name minus `.json` is the sidecar id), so an unregistered snapshot with no
    // version is silently skipped.
    let sidecar_id = spec.name.strip_suffix(".json").unwrap_or(spec.name);
    if let Some(ours) = ados_protocol::contracts::sidecar_version(sidecar_id) {
        let got = json.get("version").and_then(Value::as_u64).unwrap_or(0) as u16;
        ados_protocol::sidecar::check_sidecar_version(sidecar_id, got, ours);
    }
    for s in spec.scalars {
        if let Some(value) = lookup_number(&json, s.path) {
            let mut frame = TelemetryFrame::new(now_us, s.key, value);
            frame
                .tags
                .insert("source".to_string(), rmpv::Value::from(spec.source));
            out.push(IngestFrame::Telemetry(frame));
        }
    }
    let evt = spec.string_event.as_ref()?;
    emit_string_event(
        &json,
        spec,
        evt,
        now_us,
        last_transition,
        last_string_event_us,
        out,
    )
}

/// Emit the string-carrying snapshot event for a fresh sidecar. It fires when the
/// transition field changed since the last emission OR when the keep-alive
/// interval has elapsed since `last_string_event_us` (so a steady snapshot
/// re-emits often enough to stay inside the query window). The event detail
/// carries the file name (matching the stale/drop events) plus every present
/// string in `evt.fields`. Returns `(value, now_us)` when the event fired, else
/// `None`.
fn emit_string_event(
    json: &Value,
    spec: &SidecarSpec,
    evt: &StringEventSpec,
    now_us: i64,
    last_transition: Option<&str>,
    last_string_event_us: Option<i64>,
    out: &mut Vec<IngestFrame>,
) -> Option<(String, i64)> {
    let current = lookup_string(json, evt.transition_on)?;
    let changed = Some(current.as_str()) != last_transition;
    // Keep-alive: re-emit a steady snapshot once the interval has elapsed since
    // the last emission. A never-emitted snapshot (no prior timestamp) always
    // emits on its first fresh poll.
    let keepalive_due = match last_string_event_us {
        Some(prev) => now_us.saturating_sub(prev) >= STRING_EVENT_KEEPALIVE.as_micros() as i64,
        None => true,
    };
    if !changed && !keepalive_due {
        // Same snapshot identity and within the keep-alive window: do not re-fire.
        return None;
    }
    let mut ev = EventFrame::new(now_us, evt.kind, SOURCE_SIDECAR, Level::Info);
    ev.detail
        .insert("name".to_string(), rmpv::Value::from(spec.name));
    for field in evt.fields {
        if let Some(value) = lookup_string(json, field) {
            ev.detail
                .insert((*field).to_string(), rmpv::Value::from(value));
        }
    }
    out.push(IngestFrame::Event(ev));
    Some((current, now_us))
}

/// Build a `sidecar.stale` event carrying the file name and its age in seconds.
fn stale_event(name: &str, age_us: i64, now_us: i64) -> EventFrame {
    let mut ev = EventFrame::new(now_us, "sidecar.stale", SOURCE_SIDECAR, Level::Warn);
    ev.detail
        .insert("name".to_string(), rmpv::Value::from(name));
    ev.detail
        .insert("age_s".to_string(), rmpv::Value::from(age_us / 1_000_000));
    ev
}

/// Build a `sidecar.drop` event for a file that disappeared.
fn drop_event(name: &str, now_us: i64) -> EventFrame {
    let mut ev = EventFrame::new(now_us, "sidecar.drop", SOURCE_SIDECAR, Level::Warn);
    ev.detail
        .insert("name".to_string(), rmpv::Value::from(name));
    ev
}

/// The file's modification time as microseconds since the epoch, or `None` when
/// the file is absent or its mtime is unreadable.
fn file_mtime_us(path: &Path) -> Option<i64> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    let dur = mtime.duration_since(UNIX_EPOCH).ok()?;
    Some(dur.as_micros() as i64)
}

/// Current microsecond-epoch time from the system clock. A clock before the
/// epoch yields zero rather than a negative time.
fn now_us_systemtime() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// Resolve a dotted path to an `f64`, accepting JSON numbers and booleans.
fn lookup_number(root: &Value, path: &str) -> Option<f64> {
    let mut cur = root;
    for seg in path.split('.') {
        cur = cur.get(seg)?;
    }
    match cur {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// Resolve a dotted path to an owned `String`, accepting only JSON strings. A
/// null or absent field yields `None` so an empty snapshot carries no detail key.
fn lookup_string(root: &Value, path: &str) -> Option<String> {
    let mut cur = root;
    for seg in path.split('.') {
        cur = cur.get(seg)?;
    }
    match cur {
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    /// Write a sidecar file with `body`. Staleness is driven by the injected
    /// `now_us`, not by rewriting mtimes (which std cannot do without a new
    /// dependency): a poll with a present `now` reads a just-written file as
    /// fresh, and a poll with a `now` far in the future reads it as stale. The
    /// `_age` argument keeps the call sites self-documenting about intent.
    fn write_with_age(root: &Path, name: &str, body: &str, _age: Duration) {
        fs::write(root.join(name), body).unwrap();
    }

    fn events<'a>(frames: &'a [IngestFrame], kind: &str) -> Vec<&'a EventFrame> {
        frames
            .iter()
            .filter_map(|f| match f {
                IngestFrame::Event(e) if e.kind == kind => Some(e),
                _ => None,
            })
            .collect()
    }

    fn metric<'a>(frames: &'a [IngestFrame], key: &str) -> Option<&'a TelemetryFrame> {
        frames.iter().find_map(|f| match f {
            IngestFrame::Telemetry(t) if t.metric == key => Some(t),
            _ => None,
        })
    }

    /// A `now` value far enough in the future that any freshly-written file is
    /// older than its staleness budget.
    fn future_now() -> i64 {
        now_us_systemtime() + Duration::from_secs(3600).as_micros() as i64
    }

    /// A full air-pipeline snapshot body matching `AirPipelineStats.to_dict()`
    /// plus the publisher's `updated_at_ms`. The string fields drive the
    /// `video.air_state` event; the numerics drive the `video.air.*` series.
    const AIR_PIPELINE_BODY: &str = r#"{
        "camera_source": "v4l2src",
        "encoder_name": "v4l2h264enc",
        "encoder_hw_accel": true,
        "pipeline_state": "playing",
        "started_at": 1234.5,
        "last_state_change_at": 1240.0,
        "encoder_fps": 30.0,
        "encoded_kbps": 6000.0,
        "sei_injected_count": 12,
        "udp_bytes_out": 4096,
        "last_buffer_at": 1245.0,
        "restart_count": 1,
        "tx_silent_kicks": 0,
        "bus_errors": 0,
        "cloud_branch_open": false,
        "updated_at_ms": 1717000000000
    }"#;

    #[test]
    fn fresh_sidecars_sample_their_scalars_with_the_source_tag() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_with_age(
            root,
            "wfb-stats.json",
            r#"{"rssi": -44, "snr": 21, "fec_loss": 0.02, "bitrate_mbps": 8.5}"#,
            Duration::ZERO,
        );
        write_with_age(root, "air-pipeline.json", AIR_PIPELINE_BODY, Duration::ZERO);
        let mut state = TailerState::default();
        // A present `now` keeps the just-written files fresh.
        let frames = poll_once(root, &mut state, now_us_systemtime());

        assert_eq!(
            metric(&frames, "link.rssi.dbm").map(|m| m.value),
            Some(-44.0)
        );
        assert_eq!(metric(&frames, "link.snr.db").map(|m| m.value), Some(21.0));
        assert_eq!(
            metric(&frames, "link.fec.loss_rate").map(|m| m.value),
            Some(0.02)
        );
        // The source tag distinguishes radio from video metrics.
        assert_eq!(
            metric(&frames, "link.rssi.dbm")
                .and_then(|m| m.tags.get("source"))
                .and_then(|v| v.as_str()),
            Some("wfb")
        );
        // Nothing was stale or dropped on a fresh poll.
        assert!(events(&frames, "sidecar.stale").is_empty());
        assert!(events(&frames, "sidecar.drop").is_empty());
    }

    #[test]
    fn air_pipeline_samples_every_numeric_field_and_coerces_bools() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_with_age(root, "air-pipeline.json", AIR_PIPELINE_BODY, Duration::ZERO);
        let mut state = TailerState::default();
        let frames = poll_once(root, &mut state, now_us_systemtime());

        // Every numeric / bool field of the snapshot becomes a `video.air.*`
        // metric (bools coerced to 0/1 by `lookup_number`).
        assert_eq!(
            metric(&frames, "video.air.encoder_fps").map(|m| m.value),
            Some(30.0)
        );
        assert_eq!(
            metric(&frames, "video.air.encoded_kbps").map(|m| m.value),
            Some(6000.0)
        );
        assert_eq!(
            metric(&frames, "video.air.sei_injected_count").map(|m| m.value),
            Some(12.0)
        );
        assert_eq!(
            metric(&frames, "video.air.udp_bytes_out").map(|m| m.value),
            Some(4096.0)
        );
        assert_eq!(
            metric(&frames, "video.air.restart_count").map(|m| m.value),
            Some(1.0)
        );
        assert_eq!(
            metric(&frames, "video.air.updated_at_ms").map(|m| m.value),
            Some(1_717_000_000_000.0)
        );
        assert_eq!(
            metric(&frames, "video.air.encoder_hw_accel").map(|m| m.value),
            Some(1.0)
        );
        assert_eq!(
            metric(&frames, "video.air.cloud_branch_open").map(|m| m.value),
            Some(0.0)
        );
        // The string fields are not sampled as metrics.
        assert!(metric(&frames, "video.air.pipeline_state").is_none());
    }

    #[test]
    fn air_state_event_carries_the_strings_and_fires_once_per_transition() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_with_age(root, "air-pipeline.json", AIR_PIPELINE_BODY, Duration::ZERO);
        let mut state = TailerState::default();

        // First fresh poll fires the state event with all three strings.
        let first = poll_once(root, &mut state, now_us_systemtime());
        let evts = events(&first, "video.air_state");
        assert_eq!(evts.len(), 1);
        let d = &evts[0].detail;
        assert_eq!(
            d.get("pipeline_state").and_then(|v| v.as_str()),
            Some("playing")
        );
        assert_eq!(
            d.get("encoder_name").and_then(|v| v.as_str()),
            Some("v4l2h264enc")
        );
        assert_eq!(
            d.get("camera_source").and_then(|v| v.as_str()),
            Some("v4l2src")
        );

        // A second poll of the same snapshot does NOT re-fire (steady state).
        let second = poll_once(root, &mut state, now_us_systemtime());
        assert!(events(&second, "video.air_state").is_empty());

        // The state changes -> a new event fires.
        let changed = AIR_PIPELINE_BODY.replace("\"playing\"", "\"paused\"");
        write_with_age(root, "air-pipeline.json", &changed, Duration::ZERO);
        let third = poll_once(root, &mut state, now_us_systemtime());
        let evts3 = events(&third, "video.air_state");
        assert_eq!(evts3.len(), 1);
        assert_eq!(
            evts3[0]
                .detail
                .get("pipeline_state")
                .and_then(|v| v.as_str()),
            Some("paused")
        );
    }

    #[test]
    fn string_event_re_emits_on_the_keepalive_cadence_under_steady_state() {
        // The keep-alive path is driven by `now_us` vs the last emit time, which
        // is unit-testable directly without the mtime-vs-now tension `poll_once`
        // has (the staleness check is mtime-driven, the keep-alive is now-driven).
        // The air-pipeline spec carries the `video.air_state` string event.
        let spec = SPECS
            .iter()
            .find(|s| s.name == "air-pipeline.json")
            .expect("the air-pipeline sidecar spec is present");
        let evt = spec.string_event.as_ref().unwrap();
        let json: Value = serde_json::from_str(AIR_PIPELINE_BODY).unwrap();
        let base = 1_000_000_000_i64;
        let keepalive_us = STRING_EVENT_KEEPALIVE.as_micros() as i64;

        // Same transition value, last emitted < keep-alive ago -> no re-emit.
        let mut out = Vec::new();
        let within = emit_string_event(
            &json,
            spec,
            evt,
            base + keepalive_us - 1,
            Some("playing"),
            Some(base),
            &mut out,
        );
        assert!(within.is_none(), "no re-emit inside the keep-alive window");
        assert!(events(&out, "video.air_state").is_empty());

        // Same transition value, last emitted >= keep-alive ago -> re-emit, and
        // the returned timestamp is the new `now_us` so the next window restarts.
        let mut out = Vec::new();
        let due = emit_string_event(
            &json,
            spec,
            evt,
            base + keepalive_us,
            Some("playing"),
            Some(base),
            &mut out,
        );
        assert_eq!(due, Some(("playing".to_string(), base + keepalive_us)));
        let evts = events(&out, "video.air_state");
        assert_eq!(evts.len(), 1, "the steady snapshot re-emits on keep-alive");
        assert_eq!(
            evts[0]
                .detail
                .get("pipeline_state")
                .and_then(|v| v.as_str()),
            Some("playing")
        );

        // A never-emitted snapshot (no prior timestamp) always emits first time.
        let mut out = Vec::new();
        let first = emit_string_event(&json, spec, evt, base, None, None, &mut out);
        assert_eq!(first, Some(("playing".to_string(), base)));
        assert_eq!(events(&out, "video.air_state").len(), 1);
    }

    #[test]
    fn lcd_latency_samples_numerics_and_carries_source() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_with_age(
            root,
            "lcd-latency.json",
            r#"{"latency_ms": 42.5, "latency_ewma_ms": 40.1, "pipeline_latency_ms": null, "samples": 7, "source": "sei"}"#,
            Duration::ZERO,
        );
        let mut state = TailerState::default();
        let frames = poll_once(root, &mut state, now_us_systemtime());

        assert_eq!(
            metric(&frames, "video.latency.glass_ms").map(|m| m.value),
            Some(42.5)
        );
        assert_eq!(
            metric(&frames, "video.latency.ewma_ms").map(|m| m.value),
            Some(40.1)
        );
        assert_eq!(
            metric(&frames, "video.latency.samples").map(|m| m.value),
            Some(7.0)
        );
        // pipeline_latency_ms is null -> no metric (a null is not a number).
        assert!(metric(&frames, "video.latency.pipeline_ms").is_none());
        // The source rides the latency-source event.
        let evts = events(&frames, "video.latency_source");
        assert_eq!(evts.len(), 1);
        assert_eq!(
            evts[0].detail.get("source").and_then(|v| v.as_str()),
            Some("sei")
        );
    }

    #[test]
    fn lcd_latency_display_writer_blob_records_nothing_wrong() {
        // The display service writes an unrelated framebuffer-stats blob to the
        // same path on an LCD node. None of the SEI keys are present, so the
        // number lookups miss and the source event does not fire.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_with_age(
            root,
            "lcd-latency.json",
            r#"{"writes": 100, "drops": 0, "skipped_duplicates": 3, "last_write_ms": 12.0}"#,
            Duration::ZERO,
        );
        let mut state = TailerState::default();
        let frames = poll_once(root, &mut state, now_us_systemtime());
        assert!(metric(&frames, "video.latency.glass_ms").is_none());
        assert!(metric(&frames, "video.latency.samples").is_none());
        assert!(events(&frames, "video.latency_source").is_empty());
    }

    #[test]
    fn a_stale_file_fires_one_event_per_episode_not_per_poll() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_with_age(root, "wfb-stats.json", r#"{"rssi": -44}"#, Duration::ZERO);
        let mut state = TailerState::default();
        let now = future_now(); // makes the file read as stale

        // First stale poll fires the event.
        let first = poll_once(root, &mut state, now);
        let s1 = events(&first, "sidecar.stale");
        assert_eq!(s1.len(), 1);
        assert_eq!(
            s1[0].detail.get("name").and_then(|v| v.as_str()),
            Some("wfb-stats.json")
        );
        assert!(s1[0].detail.get("age_s").and_then(|v| v.as_i64()).unwrap() >= 3600);
        // A stale file is not sampled.
        assert!(metric(&first, "link.rssi.dbm").is_none());

        // Second stale poll does NOT re-fire: one event per episode.
        let second = poll_once(root, &mut state, now);
        assert!(events(&second, "sidecar.stale").is_empty());
    }

    #[test]
    fn a_file_recovering_from_stale_can_fire_a_fresh_episode_again() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_with_age(root, "wfb-stats.json", r#"{"rssi": -44}"#, Duration::ZERO);
        let mut state = TailerState::default();

        // Stale episode 1.
        let stale1 = poll_once(root, &mut state, future_now());
        assert_eq!(events(&stale1, "sidecar.stale").len(), 1);
        // Recover: a present `now` reads the file as fresh and resets the episode.
        let fresh = poll_once(root, &mut state, now_us_systemtime());
        assert!(events(&fresh, "sidecar.stale").is_empty());
        assert_eq!(
            metric(&fresh, "link.rssi.dbm").map(|m| m.value),
            Some(-44.0)
        );
        // Stale episode 2 fires a new event.
        let stale2 = poll_once(root, &mut state, future_now());
        assert_eq!(events(&stale2, "sidecar.stale").len(), 1);
    }

    #[test]
    fn a_disappearing_file_fires_drop_only_after_having_been_present() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let path = root.join("wfb-stats.json");
        fs::write(&path, r#"{"rssi": -44}"#).unwrap();
        let mut state = TailerState::default();

        // First poll: present and fresh, no drop.
        let first = poll_once(root, &mut state, now_us_systemtime());
        assert!(events(&first, "sidecar.drop").is_empty());

        // Remove it: the next poll fires exactly one drop event.
        fs::remove_file(&path).unwrap();
        let second = poll_once(root, &mut state, now_us_systemtime());
        let drops = events(&second, "sidecar.drop");
        assert_eq!(drops.len(), 1);
        assert_eq!(
            drops[0].detail.get("name").and_then(|v| v.as_str()),
            Some("wfb-stats.json")
        );

        // A file that was never present does not fire a drop on first sight.
        // (peer-presence.json was absent on both polls and produced nothing.)
        assert!(events(&first, "sidecar.drop").is_empty());
        let third = poll_once(root, &mut state, now_us_systemtime());
        assert!(events(&third, "sidecar.drop").is_empty(), "no repeat drop");
    }

    #[test]
    fn a_malformed_fresh_file_yields_no_metric_and_no_crash() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("wfb-stats.json"), b"{ not json").unwrap();
        let mut state = TailerState::default();
        let frames = poll_once(root, &mut state, now_us_systemtime());
        // No metric was sampled, and the file is tracked as present.
        assert!(metric(&frames, "link.rssi.dbm").is_none());
        assert!(state.entry("wfb-stats.json").present);
    }

    #[test]
    fn an_empty_root_produces_no_frames() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = TailerState::default();
        let frames = poll_once(dir.path(), &mut state, now_us_systemtime());
        assert!(frames.is_empty(), "no sidecars, no frames");
    }

    #[tokio::test]
    async fn run_sidecar_tailer_emits_a_metric_then_stops_on_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        fs::write(root.join("wfb-stats.json"), r#"{"rssi": -50}"#).unwrap();

        let (tx, mut rx) = mpsc::channel::<IngestFrame>(64);
        let (stop, shutdown) = Shutdown::pair();
        let handle = tokio::spawn(run_sidecar_tailer(root, tx, shutdown));

        // The first poll fires immediately (interval tick yields at t=0), so a
        // metric lands within a couple of poll intervals.
        let mut saw = false;
        for _ in 0..30 {
            match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                Ok(Some(IngestFrame::Telemetry(t))) if t.metric == "link.rssi.dbm" => {
                    assert_eq!(t.value, -50.0);
                    saw = true;
                    break;
                }
                Ok(Some(_)) => continue,
                _ => {}
            }
        }
        assert!(saw, "the tailer sampled the fresh sidecar");

        stop.fire();
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("the tailer stops within the bound")
            .expect("the tailer task did not panic");
    }
}
