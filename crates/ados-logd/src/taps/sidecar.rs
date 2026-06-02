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

/// One numeric scalar to sample out of a sidecar: the dotted JSON path and the
/// metric key it is recorded under.
struct ScalarMap {
    path: &'static str,
    key: &'static str,
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
    },
    SidecarSpec {
        name: "air-pipeline.json",
        staleness: DEFAULT_STALENESS,
        source: "video",
        scalars: &[
            ScalarMap {
                path: "bitrate_mbps",
                key: "video.encoder.bitrate.mbps",
            },
            ScalarMap {
                path: "encode_latency_ms",
                key: "video.encode.latency.ms",
            },
            ScalarMap {
                path: "fps",
                key: "video.pipeline.fps",
            },
        ],
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
    },
    SidecarSpec {
        name: "hop-supervisor.json",
        staleness: DEFAULT_STALENESS,
        source: "hop",
        scalars: &[],
    },
    SidecarSpec {
        name: "peer-presence.json",
        staleness: DEFAULT_STALENESS,
        source: "peer",
        scalars: &[],
    },
    SidecarSpec {
        name: "mesh-state.json",
        staleness: DEFAULT_STALENESS,
        source: "mesh",
        scalars: &[],
    },
];

/// Per-file tracking carried across polls: whether the file was present last
/// time, and whether a stale event has already fired for the current stale
/// episode (so a long stall is one event, not one per poll).
#[derive(Debug, Default, Clone)]
struct FileState {
    present: bool,
    stale_reported: bool,
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
                    // Fresh: reset the episode and sample the scalars.
                    st.stale_reported = false;
                    sample_scalars(&path, spec, now_us, &mut out);
                }
                st.present = true;
            }
        }
    }
    out
}

/// Read a fresh sidecar's JSON and append one telemetry frame per present
/// numeric scalar. A missing or malformed file is skipped without an error: the
/// freshness check already passed, so a parse failure is a transient write race,
/// not a fault.
fn sample_scalars(path: &Path, spec: &SidecarSpec, now_us: i64, out: &mut Vec<IngestFrame>) {
    if spec.scalars.is_empty() {
        return;
    }
    let body = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return,
    };
    let json: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return,
    };
    for s in spec.scalars {
        if let Some(value) = lookup_number(&json, s.path) {
            let mut frame = TelemetryFrame::new(now_us, s.key, value);
            frame
                .tags
                .insert("source".to_string(), rmpv::Value::from(spec.source));
            out.push(IngestFrame::Telemetry(frame));
        }
    }
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
        write_with_age(
            root,
            "air-pipeline.json",
            r#"{"bitrate_mbps": 6.0, "encode_latency_ms": 12, "fps": 30}"#,
            Duration::ZERO,
        );
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
        assert_eq!(
            metric(&frames, "video.encoder.bitrate.mbps").map(|m| m.value),
            Some(6.0)
        );
        assert_eq!(
            metric(&frames, "video.pipeline.fps").map(|m| m.value),
            Some(30.0)
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
