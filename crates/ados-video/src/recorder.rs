//! Ground-station video recorder.
//!
//! Records the live drone video stream as it arrives at the ground node, served
//! back to clients as MP4 files. The recorder taps the local mediamtx RTSP
//! source (`rtsp://127.0.0.1:8554/main`) rather than reaching for the upstream
//! UDP payload directly. mediamtx is the canonical local source on the ground
//! side: the ground mediamtx bridge already muxes UDP → RTSP, so this recorder
//! only needs to consume RTSP and remux to MP4 with `-c copy` (no transcode).
//!
//! Lifecycle (the same shape the Python `GroundStationRecorder` had):
//!
//! * [`start`](GroundStationRecorder::start) spawns
//!   `ffmpeg -rtsp_transport tcp -i rtsp://127.0.0.1:8554/main -c copy
//!   -movflags +frag_keyframe+empty_moov+default_base_moof -flush_packets 1
//!   <path>.mp4` as its own process-group leader. The fragmented-MP4 muxer is
//!   the point: whatever reached disk when the power went is playable.
//! * [`stop`](GroundStationRecorder::stop) sends `SIGTERM` to the group, waits
//!   up to 5s, escalates to `SIGKILL` on timeout, and reports duration + size,
//!   every segment file and how many times the capture was interrupted.
//! * [`is_active`](GroundStationRecorder::is_active) reports whether a capture is
//!   wanted; the running process holds the child across the separate start and
//!   stop HTTP requests.
//!
//! A capture is supervised. ffmpeg exits on RTSP EOF, which is exactly what a
//! degrading link or a ground mediamtx restart produces, and that is the moment
//! the recording matters most. While the capture is wanted, a supervisor task
//! checks the child every [`SUPERVISE_INTERVAL`] and, when it has exited,
//! starts the next segment `<name>_part<N>.mp4`, retrying at the same fixed
//! cadence until the source is back.
//!
//! This module is a long-lived in-process owner of the ffmpeg child: the start
//! and stop come on separate HTTP requests, so the running host process holds the
//! [`GroundStationRecorder`] across them (the recordings listing reads the same
//! directory directly and is stateless). The host process is the native HTTP
//! front, which is the cross-profile long-lived daemon that serves these routes
//! on a ground station; this crate carries only the recorder lifecycle logic.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::process::ManagedProcess;

/// How often the supervisor checks the capture while it is wanted.
pub const SUPERVISE_INTERVAL: Duration = Duration::from_secs(2);

/// The recordings directory the captures are written to. The Python recorder
/// defaulted to `RECORDINGS_DIR` (`/var/ados/recordings`); the env override lets
/// a test redirect it at a tempdir without touching the on-box path. It is the
/// same directory + override the native front's recordings-listing route reads.
pub const RECORDINGS_DIR: &str = "/var/ados/recordings";

/// Local RTSP source published by the ground mediamtx bridge. Kept in sync with
/// the ground RTSP path + port the Python recorder consumed.
pub const DEFAULT_RTSP_URL: &str = "rtsp://127.0.0.1:8554/main";

/// SIGTERM grace before escalating to SIGKILL, matching the Python recorder's 5s.
const SIGTERM_GRACE: Duration = Duration::from_secs(5);

/// Minimum free space on the recordings volume before a start is refused, matching
/// the Python recorder's `< 64 MiB` guard.
const MIN_FREE_BYTES: u64 = 64 * 1024 * 1024;

/// MP4 muxer flags that make an interrupted capture playable.
///
/// A fragmented MP4 carries an index per fragment, so the file is valid at
/// every fragment boundary. The alternative — a plain or `+faststart` MP4 —
/// writes its `moov` atom only when the muxer is closed cleanly, which yields
/// a ZERO-recoverable file on a power cut, a SIGKILL or a full disk. Air-side
/// recording gets the same property from mediamtx's native `recordFormat:
/// fmp4` (see [`crate::mediamtx`]); this is the ground-side equivalent for the
/// operator-driven capture.
const RECORDER_MOVFLAGS: &str = "+frag_keyframe+empty_moov+default_base_moof";

/// A recoverable recorder failure, carrying the stable error code the route maps
/// to an HTTP status. The codes are the exact Python `RecorderError` codes:
/// `E_RECORDING_ACTIVE` / `E_RECORDING_NOT_ACTIVE` (409),
/// `E_FFMPEG_NOT_FOUND` / `E_RECORDER_SPAWN_FAILED` / `E_RECORDING_DIR_UNWRITABLE`
/// (503), `E_DISK_FULL` (507).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecorderError {
    pub code: String,
    pub message: String,
}

impl RecorderError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for RecorderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for RecorderError {}

/// The mutable recorder state, held behind a `Mutex` so concurrent start/stop
/// requests (and the supervisor) serialize the same way the Python recorder's
/// `asyncio.Lock` did.
#[derive(Default)]
struct Inner {
    /// The running ffmpeg child, `None` when no capture is in flight.
    process: Option<ManagedProcess>,
    /// The output path of the first segment of the in-flight capture.
    current_path: Option<PathBuf>,
    /// Every segment written by the in-flight capture, in order.
    segments: Vec<PathBuf>,
    /// Times the capture's ffmpeg exited while the capture was wanted.
    interrupted_count: u32,
    /// The operator wants this capture running (set by start, cleared by stop).
    wanted: bool,
    /// The wall start instant of the in-flight capture, for the stop duration.
    started_at: Option<std::time::Instant>,
    /// The supervisor task of the in-flight capture.
    supervisor: Option<tokio::task::JoinHandle<()>>,
}

/// Captures the local mediamtx RTSP stream into MP4 files on disk.
///
/// One instance is held for the life of the host process. Cheap to share behind
/// an `Arc`; the inner state is `Mutex`-guarded so start and stop never race.
pub struct GroundStationRecorder {
    dir: PathBuf,
    rtsp_url: String,
    /// The ffmpeg program: `ffmpeg` resolved on `PATH`, or an explicit path.
    ffmpeg: String,
    inner: Arc<Mutex<Inner>>,
}

impl GroundStationRecorder {
    /// Build a recorder writing into `dir`, reading the RTSP source `rtsp_url`.
    pub fn new(dir: impl Into<PathBuf>, rtsp_url: impl Into<String>) -> Self {
        Self {
            dir: dir.into(),
            rtsp_url: rtsp_url.into(),
            ffmpeg: "ffmpeg".to_string(),
            inner: Arc::new(Mutex::new(Inner::default())),
        }
    }

    /// Use an explicit ffmpeg program instead of `ffmpeg` on `PATH`.
    pub fn with_ffmpeg(mut self, program: impl Into<String>) -> Self {
        self.ffmpeg = program.into();
        self
    }

    /// Build a recorder at the default recordings directory + RTSP source,
    /// honouring `ADOS_RECORDINGS_DIR` so a test/dev rig can redirect the path.
    pub fn default_recorder() -> Self {
        let dir =
            std::env::var("ADOS_RECORDINGS_DIR").unwrap_or_else(|_| RECORDINGS_DIR.to_string());
        Self::new(dir, DEFAULT_RTSP_URL)
    }

    /// True while a capture is wanted: from start until stop, including the
    /// moments between an interrupted segment and the next one.
    pub async fn is_active(&self) -> bool {
        self.inner.lock().await.wanted
    }

    /// The file name of the in-flight capture, or `None` when idle.
    pub async fn current_filename(&self) -> Option<String> {
        let inner = self.inner.lock().await;
        inner
            .current_path
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(str::to_string)
    }

    /// Spawn ffmpeg to record the local RTSP stream.
    ///
    /// Returns `{filename, started_at, path}` on success — `started_at` is an
    /// ISO-8601 UTC timestamp, the same fields the Python `start()` returned.
    /// Raises a [`RecorderError`] (with the Python error code) when start cannot
    /// proceed: already recording (409), ffmpeg missing / dir unwritable / spawn
    /// failed (503), or the volume is full (507).
    pub async fn start(&self, filename_hint: Option<&str>) -> Result<Value, RecorderError> {
        let mut inner = self.inner.lock().await;

        // Already recording → 409, the same guard the Python recorder applied
        // under its lock.
        if inner.wanted {
            return Err(RecorderError::new(
                "E_RECORDING_ACTIVE",
                "a recording is already in progress",
            ));
        }

        // ffmpeg presence → 503 when absent (the Python `shutil.which` check).
        if !program_present(&self.ffmpeg) {
            return Err(RecorderError::new(
                "E_FFMPEG_NOT_FOUND",
                "ffmpeg binary not on PATH",
            ));
        }

        // Create the recordings directory; a failure is the unwritable-dir 503.
        if let Err(e) = std::fs::create_dir_all(&self.dir) {
            return Err(RecorderError::new(
                "E_RECORDING_DIR_UNWRITABLE",
                format!("cannot create recordings directory: {e}"),
            ));
        }

        // Disk-space guard → 507 under the same 64 MiB floor the Python used. A
        // stat failure is non-fatal (the Python treated `free_bytes is None` as
        // "do not block"), so only a known-low free count refuses.
        if let Some(free) = free_bytes(&self.dir) {
            if free < MIN_FREE_BYTES {
                return Err(RecorderError::new(
                    "E_DISK_FULL",
                    "less than 64 MiB free on the recordings volume",
                ));
            }
        }

        let filename = generate_filename(filename_hint);
        let output_path = self.dir.join(&filename);
        let process = spawn_capture(&self.ffmpeg, &self.rtsp_url, &output_path).map_err(|e| {
            RecorderError::new(
                "E_RECORDER_SPAWN_FAILED",
                format!("failed to spawn ffmpeg: {e}"),
            )
        })?;

        inner.process = Some(process);
        inner.current_path = Some(output_path.clone());
        inner.segments = vec![output_path.clone()];
        inner.interrupted_count = 0;
        inner.wanted = true;
        inner.started_at = Some(std::time::Instant::now());
        inner.supervisor = Some(tokio::spawn(supervise(
            Arc::clone(&self.inner),
            self.ffmpeg.clone(),
            self.rtsp_url.clone(),
            output_path.clone(),
        )));

        tracing::info!(
            filename = %filename,
            path = %output_path.display(),
            rtsp_url = %self.rtsp_url,
            "recording started"
        );

        Ok(json!({
            "filename": filename,
            "started_at": now_iso8601(),
            "path": output_path.to_string_lossy(),
        }))
    }

    /// Gracefully stop the in-flight recording.
    ///
    /// Stops the supervisor, then sends `SIGTERM` to the ffmpeg process group,
    /// waits up to 5s and escalates to `SIGKILL` on timeout. Returns
    /// `{filename, stopped_at, duration_seconds, size_bytes, segments,
    /// interrupted_count}`: `filename` is the first segment, `size_bytes` the
    /// total over every segment, `segments` every segment file name in order.
    /// Raises a [`RecorderError`] (`E_RECORDING_NOT_ACTIVE`, 409) when no
    /// recording is active.
    pub async fn stop(&self) -> Result<Value, RecorderError> {
        let mut inner = self.inner.lock().await;
        if !inner.wanted {
            return Err(RecorderError::new(
                "E_RECORDING_NOT_ACTIVE",
                "no recording is currently active",
            ));
        }
        inner.wanted = false;
        if let Some(h) = inner.supervisor.take() {
            h.abort();
        }

        // SIGTERM → wait(grace) → SIGKILL of the whole process group.
        if let Some(mut process) = inner.process.take() {
            process.terminate(SIGTERM_GRACE).await;
        }

        let duration = inner
            .started_at
            .take()
            .map(|s| s.elapsed().as_secs_f64())
            .unwrap_or(0.0)
            .max(0.0);
        let path = inner.current_path.take();
        let segments = std::mem::take(&mut inner.segments);
        let interrupted_count = inner.interrupted_count;

        let filename = path
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("unknown.mp4")
            .to_string();
        let size_bytes: u64 = segments
            .iter()
            .filter_map(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .sum();
        let segment_names: Vec<String> = segments
            .iter()
            .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_string))
            .collect();

        tracing::info!(
            filename = %filename,
            duration_s = round2(duration),
            size_bytes,
            segments = segment_names.len(),
            interrupted_count,
            "recording stopped"
        );

        Ok(json!({
            "filename": filename,
            "stopped_at": now_iso8601(),
            "duration_seconds": round2(duration),
            "size_bytes": size_bytes,
            "segments": segment_names,
            "interrupted_count": interrupted_count,
        }))
    }
}

/// Spawn one capture segment into `output_path`.
///
/// Reads the local RTSP source over TCP and remuxes to MP4 with `-c copy` (no
/// transcode). The muxer flags are the crash-safety contract, not tuning. A
/// plain (or `+faststart`) MP4 writes its `moov` atom only on a clean exit, so
/// a ground station that loses power, is SIGKILLed or fills its disk
/// mid-capture leaves a file with no index at all — not a truncated
/// recording, a zero-recoverable one.
///
/// `empty_moov` puts a valid (empty) index at byte 0 the moment the file is
/// created; `frag_keyframe` closes a self-describing `moof`+`mdat` fragment at
/// every IDR, so with the shipped 0.5 s GOP at most half a second of video is
/// ever un-indexed; `default_base_moof` makes each fragment's offsets relative
/// to its own `moof`. `-flush_packets 1` pushes each packet out of ffmpeg's
/// AVIO buffer, which bounds the loss to the current GOP.
///
/// Spawned as a process-group leader so the whole pipeline is reaped on stop.
/// ffmpeg's stderr is drained: a pipe nobody reads fills at 64 KB and blocks
/// the muxer's next write.
fn spawn_capture(
    ffmpeg: &str,
    rtsp_url: &str,
    output_path: &Path,
) -> std::io::Result<ManagedProcess> {
    let args: Vec<String> = vec![
        "-y".to_string(),
        "-rtsp_transport".to_string(),
        "tcp".to_string(),
        "-i".to_string(),
        rtsp_url.to_string(),
        "-c".to_string(),
        "copy".to_string(),
        "-movflags".to_string(),
        RECORDER_MOVFLAGS.to_string(),
        "-flush_packets".to_string(),
        "1".to_string(),
        output_path.to_string_lossy().to_string(),
    ];
    let mut process = ManagedProcess::spawn("gs-recorder", ffmpeg, &args)?;
    if let Some(stderr) = process.take_stderr() {
        tokio::spawn(crate::stderr_drain::drain_plain(stderr, "gs-recorder"));
    }
    Ok(process)
}

/// `<stem>_part<n>.mp4` beside `first`.
fn segment_path(first: &Path, n: usize) -> PathBuf {
    let stem = first
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("recording");
    first.with_file_name(format!("{stem}_part{n}.mp4"))
}

/// Keep a wanted capture running: every [`SUPERVISE_INTERVAL`], if the ffmpeg
/// child has exited (or a previous respawn failed), start the next segment.
async fn supervise(inner: Arc<Mutex<Inner>>, ffmpeg: String, rtsp_url: String, first: PathBuf) {
    loop {
        tokio::time::sleep(SUPERVISE_INTERVAL).await;
        let mut guard = inner.lock().await;
        if !guard.wanted {
            return;
        }
        let alive = guard.process.as_mut().is_some_and(|p| p.is_running());
        if alive {
            continue;
        }
        if guard.process.take().is_some() {
            guard.interrupted_count += 1;
            tracing::warn!(
                interrupted_count = guard.interrupted_count,
                "recording_interrupted: capture exited while wanted; starting a new segment"
            );
        }
        let next = segment_path(&first, guard.segments.len() + 1);
        match spawn_capture(&ffmpeg, &rtsp_url, &next) {
            Ok(p) => {
                tracing::info!(path = %next.display(), "recording_segment_started");
                guard.process = Some(p);
                guard.segments.push(next);
            }
            Err(e) => {
                tracing::warn!(error = %e, "recording_segment_spawn_failed; retrying");
            }
        }
    }
}

/// True when `program` can be run: an explicit path must be a file; a bare
/// name must resolve on `PATH` (the Python `shutil.which` check).
fn program_present(program: &str) -> bool {
    if program.contains('/') {
        return Path::new(program).is_file();
    }
    let Ok(path_var) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|dir| dir.join(program).is_file())
}

/// The free bytes on the volume holding `dir`, or `None` when the stat fails (a
/// `None` is treated as "do not block" at the call site, matching the Python
/// `shutil.disk_usage` `except OSError` path). Linux uses `statvfs`; off Linux
/// the dev host returns `None` so a test never refuses on a disk-space guard.
#[cfg(target_os = "linux")]
fn free_bytes(dir: &Path) -> Option<u64> {
    let stat = nix::sys::statvfs::statvfs(dir).ok()?;
    Some(stat.blocks_available() as u64 * stat.fragment_size() as u64)
}

#[cfg(not(target_os = "linux"))]
fn free_bytes(_dir: &Path) -> Option<u64> {
    None
}

/// A timestamped MP4 filename, with a sanitised hint appended. Matches the Python
/// recorder's `%Y-%m-%dT%H-%M-%S` UTC stamp + `_<hint>` (alnum / `-` / `_` only,
/// truncated to 48 chars). A hint that sanitises to empty is dropped.
fn generate_filename(hint: Option<&str>) -> String {
    let ts = utc_filename_stamp();
    match hint {
        Some(h) if !h.is_empty() => {
            let safe: String = h
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .take(48)
                .collect();
            if safe.is_empty() {
                format!("{ts}.mp4")
            } else {
                format!("{ts}_{safe}.mp4")
            }
        }
        _ => format!("{ts}.mp4"),
    }
}

/// The current UTC time as the recorder's `%Y-%m-%dT%H-%M-%S` filename stamp.
fn utc_filename_stamp() -> String {
    use time::format_description::FormatItem;
    use time::macros::format_description;
    const FMT: &[FormatItem<'_>] =
        format_description!("[year]-[month]-[day]T[hour]-[minute]-[second]");
    time::OffsetDateTime::now_utc()
        .format(FMT)
        .unwrap_or_else(|_| "0000-00-00T00-00-00".to_string())
}

/// The current UTC time as an ISO-8601 timestamp with a `+00:00` offset, matching
/// the Python `datetime.now(timezone.utc).isoformat()` the start/stop returned.
fn now_iso8601() -> String {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map(|s| {
            // RFC3339 renders UTC as a trailing `Z`; Python's isoformat renders
            // `+00:00`. Match the Python form so the wire value is identical.
            if let Some(stripped) = s.strip_suffix('Z') {
                format!("{stripped}+00:00")
            } else {
                s
            }
        })
        .unwrap_or_else(|_| "1970-01-01T00:00:00+00:00".to_string())
}

/// Round a duration to 2 decimal places, matching the Python `round(x, 2)` the
/// stop summary used.
fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::Mutex;

    /// Serializes the tests that mutate the process-global `PATH` so a parallel
    /// test in this crate cannot clobber it mid-flight. Held for the whole body of
    /// each PATH-mutating test. A `tokio` Mutex so the guard is safe to hold across
    /// the `.await` points in the recorder start/stop calls.
    static PATH_LOCK: Mutex<()> = Mutex::const_new(());

    #[test]
    fn filename_has_no_hint_when_absent() {
        let name = generate_filename(None);
        assert!(name.ends_with(".mp4"));
        // No underscore-separated hint segment.
        assert!(!name.trim_end_matches(".mp4").contains('_'));
    }

    #[test]
    fn filename_appends_a_sanitised_hint() {
        let name = generate_filename(Some("test flight!@#"));
        assert!(name.ends_with("_testflight.mp4"), "got {name}");
    }

    #[test]
    fn filename_keeps_dash_and_underscore_in_the_hint() {
        let name = generate_filename(Some("pipe-run_2"));
        assert!(name.ends_with("_pipe-run_2.mp4"), "got {name}");
    }

    #[test]
    fn filename_drops_a_hint_that_sanitises_to_empty() {
        let name = generate_filename(Some("!!!"));
        assert!(name.ends_with(".mp4"));
        assert!(!name.trim_end_matches(".mp4").contains('_'));
    }

    #[test]
    fn filename_truncates_a_long_hint_to_48_chars() {
        let long = "a".repeat(100);
        let name = generate_filename(Some(&long));
        let hint = name
            .trim_end_matches(".mp4")
            .rsplit('_')
            .next()
            .unwrap_or("");
        assert_eq!(hint.len(), 48);
    }

    #[test]
    fn iso8601_renders_a_utc_offset_not_a_z() {
        let s = now_iso8601();
        assert!(s.ends_with("+00:00"), "got {s}");
        assert!(!s.ends_with('Z'));
    }

    #[test]
    fn round2_rounds_to_two_decimals() {
        assert_eq!(round2(60.0), 60.0);
        assert_eq!(round2(1.2345), 1.23);
        assert_eq!(round2(1.235), 1.24);
    }

    #[tokio::test]
    async fn an_idle_recorder_is_not_active() {
        let dir = tempfile::tempdir().unwrap();
        let rec = GroundStationRecorder::new(dir.path(), DEFAULT_RTSP_URL);
        assert!(!rec.is_active().await);
        assert_eq!(rec.current_filename().await, None);
    }

    #[tokio::test]
    async fn stop_when_idle_is_not_active_error() {
        let dir = tempfile::tempdir().unwrap();
        let rec = GroundStationRecorder::new(dir.path(), DEFAULT_RTSP_URL);
        let err = rec.stop().await.unwrap_err();
        assert_eq!(err.code, "E_RECORDING_NOT_ACTIVE");
        assert_eq!(err.message, "no recording is currently active");
    }

    #[tokio::test]
    async fn start_without_ffmpeg_is_ffmpeg_not_found() {
        let _guard = PATH_LOCK.lock().await;
        // Empty PATH → ffmpeg is absent → the 503 error code, before any spawn.
        let path_save = std::env::var("PATH").ok();
        let dir = tempfile::tempdir().unwrap();
        let rec = GroundStationRecorder::new(dir.path(), DEFAULT_RTSP_URL);
        // Set an empty PATH for this check; restore after.
        std::env::set_var("PATH", "");
        let err = rec.start(None).await.unwrap_err();
        if let Some(p) = path_save {
            std::env::set_var("PATH", p);
        } else {
            std::env::remove_var("PATH");
        }
        assert_eq!(err.code, "E_FFMPEG_NOT_FOUND");
    }

    #[tokio::test]
    async fn start_and_stop_full_cycle_against_a_fake_ffmpeg() {
        let _guard = PATH_LOCK.lock().await;
        // Put a fake `ffmpeg` on the PATH that just sleeps, so start succeeds and
        // stop terminates it. The fake writes a few bytes to the output path it is
        // handed so the stop summary reports a real size.
        let bindir = tempfile::tempdir().unwrap();
        let fake = bindir.path().join("ffmpeg");
        std::fs::write(
            &fake,
            "#!/bin/sh\n# the last arg is the output path; write to it then sleep\nfor out; do :; done\nprintf 'data' > \"$out\"\nsleep 30\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let recdir = tempfile::tempdir().unwrap();
        let rec = GroundStationRecorder::new(recdir.path(), DEFAULT_RTSP_URL);

        let path_save = std::env::var("PATH").ok();
        // Prepend the fake dir so `ffmpeg` resolves to the fake, but keep the real
        // PATH so the fake's own `sh`/`printf`/`sleep` still resolve.
        let combined = match &path_save {
            Some(orig) => format!("{}:{}", bindir.path().display(), orig),
            None => bindir.path().display().to_string(),
        };
        std::env::set_var("PATH", &combined);

        let started = rec.start(Some("cycle")).await.expect("start succeeds");
        assert!(rec.is_active().await, "a capture is in flight after start");
        // The start body shape.
        let started_name = started["filename"].as_str().unwrap().to_string();
        assert!(started_name.ends_with("_cycle.mp4"));
        assert!(started["started_at"].as_str().unwrap().ends_with("+00:00"));
        assert!(started["path"]
            .as_str()
            .unwrap()
            .ends_with(&format!("/{started_name}")));

        // A second start while active is the 409.
        let busy = rec.start(None).await.unwrap_err();
        assert_eq!(busy.code, "E_RECORDING_ACTIVE");

        // Give the fake a moment to write the output file.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let stopped = rec.stop().await.expect("stop succeeds");
        if let Some(p) = path_save {
            std::env::set_var("PATH", p);
        } else {
            std::env::remove_var("PATH");
        }
        assert!(!rec.is_active().await, "idle after stop");
        // The stop body shape: the four fields, the filename echoed.
        assert_eq!(stopped["filename"].as_str().unwrap(), started_name);
        assert!(stopped["stopped_at"].as_str().unwrap().ends_with("+00:00"));
        assert!(stopped["duration_seconds"].as_f64().unwrap() >= 0.0);
        assert_eq!(stopped["size_bytes"].as_u64().unwrap(), 4); // "data"
    }

    /// A capture whose ffmpeg is chatty on stderr must keep writing.
    ///
    /// The fake writes far more than a pipe buffer holds to stderr BEFORE it
    /// touches the output file. With stderr piped and never read, the write
    /// blocks once the pipe fills and the output never appears — exactly the
    /// recording that silently stops growing mid-flight.
    #[tokio::test]
    async fn a_chatty_ffmpeg_keeps_recording() {
        let _guard = PATH_LOCK.lock().await;
        let bindir = tempfile::tempdir().unwrap();
        let fake = bindir.path().join("ffmpeg");
        std::fs::write(
            &fake,
            "#!/bin/sh\nfor out; do :; done\n\
             i=0; while [ $i -lt 4000 ]; do echo \"frame=$i fps=30 q=23.0 size=512kB time=00:00:01 bitrate=4000kbits/s speed=1x\" >&2; i=$((i+1)); done\n\
             printf 'data' > \"$out\"\nsleep 30\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let recdir = tempfile::tempdir().unwrap();
        let rec = GroundStationRecorder::new(recdir.path(), DEFAULT_RTSP_URL);
        let path_save = std::env::var("PATH").ok();
        let combined = match &path_save {
            Some(orig) => format!("{}:{}", bindir.path().display(), orig),
            None => bindir.path().display().to_string(),
        };
        std::env::set_var("PATH", &combined);

        let started = rec.start(Some("chatty")).await.expect("start succeeds");
        let out_path = std::path::PathBuf::from(started["path"].as_str().unwrap());
        let mut written = false;
        for _ in 0..100 {
            if std::fs::read(&out_path).is_ok_and(|b| b == b"data") {
                written = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let _ = rec.stop().await;
        if let Some(p) = path_save {
            std::env::set_var("PATH", p);
        } else {
            std::env::remove_var("PATH");
        }
        assert!(
            written,
            "the capture blocked on a full stderr pipe and never wrote its output"
        );
    }

    /// The recorder's muxer flags are a data-integrity contract, not tuning.
    ///
    /// A plain or `+faststart` MP4 writes its `moov` atom only when the muxer
    /// closes cleanly, so a ground station that loses power, is SIGKILLed or
    /// fills its disk mid-capture leaves a file `ffprobe` reports as "moov
    /// atom not found" — the whole flight recording, not its tail. This test
    /// captures the literal argv the recorder hands ffmpeg so a future
    /// simplification of the invocation cannot silently give that back.
    #[tokio::test]
    async fn the_capture_is_muxed_as_a_fragmented_mp4() {
        let _guard = PATH_LOCK.lock().await;
        let bindir = tempfile::tempdir().unwrap();
        let fake = bindir.path().join("ffmpeg");
        // Record the argv beside the output file, then idle so the capture
        // stays "in flight" while the assertions run.
        std::fs::write(
            &fake,
            "#!/bin/sh\nfor out; do :; done\nprintf '%s\\n' \"$@\" > \"$out.argv\"\nsleep 30\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let recdir = tempfile::tempdir().unwrap();
        let rec = GroundStationRecorder::new(recdir.path(), DEFAULT_RTSP_URL);

        let path_save = std::env::var("PATH").ok();
        let combined = match &path_save {
            Some(orig) => format!("{}:{}", bindir.path().display(), orig),
            None => bindir.path().display().to_string(),
        };
        std::env::set_var("PATH", &combined);

        // A script written moments ago can refuse to exec with ETXTBSY while a
        // fork running concurrently in another test still holds the write
        // descriptor it inherited. That is a test-timing artefact, not the
        // recorder's behaviour, so retry it briefly.
        let mut busy_retries = 0;
        let started = loop {
            match rec.start(Some("frag")).await {
                Ok(started) => break started,
                Err(e) if e.message.contains("Text file busy") && busy_retries < 20 => {
                    busy_retries += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Err(e) => panic!("start succeeds: {e:?}"),
            }
        };
        let out_path = started["path"].as_str().unwrap().to_string();
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let _ = rec.stop().await;
        if let Some(p) = path_save {
            std::env::set_var("PATH", p);
        } else {
            std::env::remove_var("PATH");
        }

        let argv: Vec<String> = std::fs::read_to_string(format!("{out_path}.argv"))
            .expect("the fake ffmpeg recorded its argv")
            .lines()
            .map(str::to_string)
            .collect();

        let movflags = argv
            .iter()
            .position(|a| a == "-movflags")
            .expect("the recorder passes -movflags");
        assert_eq!(argv[movflags + 1], RECORDER_MOVFLAGS);
        assert!(
            argv[movflags + 1].contains("frag_keyframe")
                && argv[movflags + 1].contains("empty_moov")
                && argv[movflags + 1].contains("default_base_moof"),
            "a capture killed mid-write is only playable with all three fMP4 flags: {argv:?}"
        );
        assert!(
            !argv.iter().any(|a| a.contains("faststart")),
            "+faststart defers the moov atom to a clean exit: {argv:?}"
        );
        let flush = argv
            .iter()
            .position(|a| a == "-flush_packets")
            .expect("the recorder flushes each packet out of the AVIO buffer");
        assert_eq!(argv[flush + 1], "1");
    }

    #[test]
    fn segments_are_numbered_beside_the_first_file() {
        let first = Path::new("/rec/2026-01-01T00-00-00_flight.mp4");
        assert_eq!(
            segment_path(first, 2),
            Path::new("/rec/2026-01-01T00-00-00_flight_part2.mp4")
        );
    }

    /// A capture whose ffmpeg exits while the recording is wanted continues in
    /// a new segment, and stop reports every segment and the interruption.
    #[tokio::test]
    async fn an_interrupted_capture_continues_in_a_new_segment() {
        // The fake's own `sleep` resolves on PATH, which sibling tests mutate.
        let _guard = PATH_LOCK.lock().await;
        let bindir = tempfile::tempdir().unwrap();
        let fake = bindir.path().join("fake-ffmpeg");
        // The first segment ends at once (an RTSP EOF); later ones keep going.
        std::fs::write(
            &fake,
            "#!/bin/sh\nfor out; do :; done\nprintf 'data' > \"$out\"\n\
             case \"$out\" in *_part*) sleep 30 ;; *) exit 1 ;; esac\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let recdir = tempfile::tempdir().unwrap();
        let rec = GroundStationRecorder::new(recdir.path(), DEFAULT_RTSP_URL)
            .with_ffmpeg(fake.to_string_lossy().into_owned());

        let mut busy_retries = 0;
        let started = loop {
            match rec.start(Some("seg")).await {
                Ok(started) => break started,
                Err(e) if e.message.contains("Text file busy") && busy_retries < 20 => {
                    busy_retries += 1;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => panic!("start succeeds: {e:?}"),
            }
        };
        let first = started["filename"].as_str().unwrap().to_string();
        let second = format!("{}_part2.mp4", first.trim_end_matches(".mp4"));
        let second_path = recdir.path().join(&second);
        let mut seen = false;
        for _ in 0..80 {
            if second_path.exists() {
                seen = true;
                break;
            }
            // Still wanted between segments.
            assert!(rec.is_active().await);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(seen, "a new segment starts after the capture exits");

        let stopped = rec.stop().await.expect("stop succeeds");
        assert_eq!(stopped["filename"].as_str().unwrap(), first);
        assert_eq!(stopped["segments"], json!([first, second]));
        assert_eq!(stopped["interrupted_count"], 1);
        assert_eq!(stopped["size_bytes"].as_u64().unwrap(), 8);
        assert!(!rec.is_active().await);
    }
}
