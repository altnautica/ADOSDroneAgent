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

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::process::ManagedProcess;

mod capture;

use capture::{
    free_bytes, generate_filename, now_iso8601, program_present, round2, spawn_capture, supervise,
};

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

#[cfg(test)]
mod tests;
