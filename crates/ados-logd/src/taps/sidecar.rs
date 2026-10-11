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
use ados_protocol::paths;

use super::{Shutdown, SOURCE_SIDECAR};

/// How often the directory is polled.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

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
/// fields (the SEI tap's `source`, today the only one) needs a second carrier. This describes that carrier: an event
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
    /// Freshness budget before a `sidecar.stale` event fires: the sidecar's
    /// registry window (`ados_protocol::paths::*_MAX_AGE_MS`).
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
/// each. File names and windows both come from the generated path registry, so
/// the tailer and every other reader agree on when a file stops being current.
const SPECS: &[SidecarSpec] = &[
    SidecarSpec {
        name: paths::WFB_STATS_JSON,
        staleness: Duration::from_millis(paths::WFB_STATS_MAX_AGE_MS),
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
    // The SEI glass-to-glass latency snapshot written by the drone-side tap when
    // SEI latency is enabled. The numeric fields ride a `video.latency.*` series;
    // `source` rides a `video.latency_source` event.
    SidecarSpec {
        name: paths::LCD_LATENCY_JSON,
        staleness: Duration::from_millis(paths::LCD_LATENCY_MAX_AGE_MS),
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
        name: paths::CAMERA_STATE_JSON,
        staleness: Duration::from_millis(paths::CAMERA_STATE_MAX_AGE_MS),
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
        name: paths::HOP_SUPERVISOR_JSON,
        staleness: Duration::from_millis(paths::HOP_SUPERVISOR_MAX_AGE_MS),
        source: "hop",
        scalars: &[],
        string_event: None,
    },
    SidecarSpec {
        name: paths::PEER_PRESENCE_JSON,
        staleness: Duration::from_millis(paths::PEER_PRESENCE_MAX_AGE_MS),
        source: "peer",
        scalars: &[],
        string_event: None,
    },
    SidecarSpec {
        name: paths::MESH_STATE_JSON,
        staleness: Duration::from_millis(paths::MESH_STATE_MAX_AGE_MS),
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
mod tests;
