//! The `mediamtx` process manager: config writes, start/stop, per-path API queries.

use std::path::{Path, PathBuf};

use super::net::http_get;
use super::*;
use crate::process::ManagedProcess;

/// An owned snapshot of everything a config render+write needs, so the whole
/// blocking sequence (routing-table probe, `/sys/class/net` walk, `create_dir_all`,
/// `write`) runs on the blocking pool without borrowing the manager.
struct ConfigWriteJob {
    profile: MediamtxProfile,
    api_port: u16,
    rtsp_port: u16,
    webrtc_port: u16,
    hls_port: u16,
    playback_port: u16,
    recording: RecordingParams,
    config_path: PathBuf,
    streams: Vec<(String, String)>,
    /// `None` ⇒ detect at render time; `Some` ⇒ use these verbatim.
    lan_ips: Option<Vec<String>>,
}

impl ConfigWriteJob {
    fn render_and_write(self) -> std::io::Result<()> {
        let lan_ips = self.lan_ips.unwrap_or_else(detect_lan_ips);
        let yaml = mediamtx_config_yaml(&ConfigParams {
            profile: self.profile,
            api_port: self.api_port,
            rtsp_port: self.rtsp_port,
            webrtc_port: self.webrtc_port,
            hls_port: self.hls_port,
            playback_port: self.playback_port,
            lan_ips: &lan_ips,
            streams: &self.streams,
            recording: self.recording,
        });
        if let Some(parent) = self.config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.config_path, yaml)
    }
}

/// Manages a `mediamtx` subprocess for WebRTC/RTSP/HLS streaming.
pub struct MediamtxManager {
    profile: MediamtxProfile,
    api_port: u16,
    rtsp_port: u16,
    webrtc_port: u16,
    hls_port: u16,
    playback_port: u16,
    recording: RecordingParams,
    config_path: PathBuf,
    process: Option<ManagedProcess>,
}

impl MediamtxManager {
    /// Construct for the air profile with the default ports and a config path
    /// under `config_dir` (e.g. a temp dir). The config file is written by
    /// [`Self::write_config`].
    pub fn new(config_dir: &Path) -> Self {
        Self::for_profile(MediamtxProfile::Air, config_dir)
    }

    /// Construct for an explicit profile.
    pub fn for_profile(profile: MediamtxProfile, config_dir: &Path) -> Self {
        Self {
            profile,
            api_port: DEFAULT_API_PORT,
            rtsp_port: DEFAULT_RTSP_PORT,
            webrtc_port: DEFAULT_WEBRTC_PORT,
            hls_port: DEFAULT_HLS_PORT,
            playback_port: DEFAULT_PLAYBACK_PORT,
            recording: RecordingParams::default(),
            config_path: config_dir.join("mediamtx.yml"),
            process: None,
        }
    }

    /// Override the ports (tests / non-default deployments).
    pub fn with_ports(mut self, api: u16, rtsp: u16, webrtc: u16, hls: u16) -> Self {
        self.api_port = api;
        self.rtsp_port = rtsp;
        self.webrtc_port = webrtc;
        self.hls_port = hls;
        self
    }

    /// Set the recording knobs the rendered `pathDefaults` block carries.
    pub fn with_recording(mut self, recording: RecordingParams) -> Self {
        self.recording = recording;
        self
    }

    pub fn rtsp_port(&self) -> u16 {
        self.rtsp_port
    }
    pub fn webrtc_port(&self) -> u16 {
        self.webrtc_port
    }
    pub fn api_port(&self) -> u16 {
        self.api_port
    }
    pub fn playback_port(&self) -> u16 {
        self.playback_port
    }
    pub fn config_path(&self) -> &Path {
        &self.config_path
    }
    pub fn recording(&self) -> &RecordingParams {
        &self.recording
    }

    /// Render the config for `streams` and write it, off the async runtime.
    ///
    /// Every step here is blocking and none of it belongs on a reactor worker:
    /// [`detect_lan_ips`] opens a UDP socket and reads the routing table,
    /// [`physical_lan_interfaces`] walks `/sys/class/net`, and the render ends in
    /// a synchronous `create_dir_all` + `write`. This is reached from
    /// `start_stream`, so doing it inline stalls a worker that other video
    /// routes and watchdog ticks share. `spawn_blocking` moves the whole
    /// sequence to the blocking pool where a slow `/sys` read costs nothing but
    /// its own thread.
    pub async fn write_config(&self, streams: &[(String, String)]) -> std::io::Result<()> {
        let owned = self.render_inputs(streams.to_vec());
        tokio::task::spawn_blocking(move || owned.render_and_write())
            .await
            .map_err(std::io::Error::other)?
    }

    /// Render + write with the LAN IPs supplied, off the async runtime (keeps
    /// the I/O path testable without a live network).
    pub async fn write_config_with_ips(
        &self,
        streams: &[(String, String)],
        lan_ips: &[String],
    ) -> std::io::Result<()> {
        let mut owned = self.render_inputs(streams.to_vec());
        owned.lan_ips = Some(lan_ips.to_vec());
        tokio::task::spawn_blocking(move || owned.render_and_write())
            .await
            .map_err(std::io::Error::other)?
    }

    /// The owned snapshot the blocking render works from, so nothing borrows
    /// `self` across the `spawn_blocking` boundary.
    fn render_inputs(&self, streams: Vec<(String, String)>) -> ConfigWriteJob {
        ConfigWriteJob {
            profile: self.profile,
            api_port: self.api_port,
            rtsp_port: self.rtsp_port,
            webrtc_port: self.webrtc_port,
            hls_port: self.hls_port,
            playback_port: self.playback_port,
            recording: self.recording.clone(),
            config_path: self.config_path.clone(),
            streams,
            lan_ips: None,
        }
    }

    /// Spawn `mediamtx <config>` through [`ManagedProcess`] and gate on the
    /// RTSP listener accepting.
    ///
    /// `Ok(true)` means the process is up: either the RTSP gate passed, or it
    /// timed out while the process is still alive, since a slow listener may
    /// still come up and is logged rather than treated as fatal. `Ok(false)`
    /// means the process is already gone — the case a rejected config produces,
    /// and the one that must not be reported as a successful start.
    ///
    /// `write_config` must have been called first. Idempotent: a still-alive
    /// process stays.
    pub async fn start(&mut self) -> std::io::Result<bool> {
        if let Some(p) = self.process.as_mut() {
            if p.is_running() {
                return Ok(true);
            }
            self.process = None;
        }
        let config = self.config_path.to_string_lossy().to_string();
        let mut p = ManagedProcess::spawn_capturing_stdout("mediamtx", "mediamtx", &[config])?;
        // Drain both pipes in the background: mediamtx writes its log to
        // stdout, and a pipe nobody reads fills at 64 KB and blocks mediamtx's
        // next write (freezing the whole video pipeline while the process
        // still looks alive).
        if let Some(stdout) = p.take_stdout() {
            tokio::spawn(drain_mediamtx_log(stdout));
        }
        if let Some(stderr) = p.take_stderr() {
            tokio::spawn(drain_mediamtx_log(stderr));
        }
        self.process = Some(p);

        let ready = wait_for_tcp_port("127.0.0.1", self.rtsp_port, RTSP_BIND_TIMEOUT).await;
        if !ready {
            // A listener that is merely slow may still come up, so that stays
            // non-fatal. A process that has ALREADY EXITED is a different
            // thing entirely: mediamtx refuses to start at all on an
            // unrecognised config key, and exits immediately, so this is
            // exactly the shape a config it does not understand takes. Calling
            // that "started" left the whole video pipeline dark behind a single
            // log line, with every downstream surface reporting a healthy
            // service.
            if !self.is_running() {
                tracing::error!(
                    port = self.rtsp_port,
                    config = %self.config_path.display(),
                    "mediamtx_exited_before_rtsp_ready"
                );
                return Ok(false);
            }
            tracing::error!(
                port = self.rtsp_port,
                timeout_s = RTSP_BIND_TIMEOUT.as_secs(),
                "mediamtx_rtsp_port_not_ready"
            );
        }
        Ok(true)
    }

    /// True while the mediamtx process has not exited.
    pub fn is_running(&mut self) -> bool {
        match self.process.as_mut() {
            Some(p) => p.is_running(),
            None => false,
        }
    }

    /// Graceful teardown of the process group and config-file cleanup.
    pub async fn stop(&mut self) {
        if let Some(mut p) = self.process.take() {
            p.terminate(Duration::from_secs(5)).await;
        }
        let _ = std::fs::remove_file(&self.config_path);
    }

    /// Probe the API: is the named path ready? `GET /v3/paths/get/<name>` →
    /// `data["ready"] == true`. Looks the path up BY NAME (`main`), never by
    /// list index. `false` when the API is unreachable / non-200 / the field
    /// is absent.
    pub async fn path_ready(&self, name: &str) -> bool {
        let Some(body) = http_get(self.api_port, &format!("/v3/paths/get/{name}")).await else {
            return false;
        };
        let Ok(data) = serde_json::from_slice::<serde_json::Value>(&body) else {
            return false;
        };
        data.get("ready").and_then(|v| v.as_bool()).unwrap_or(false)
    }

    /// Read the cumulative `bytesReceived` counter for the named path — the
    /// authoritative "data is actually arriving from the encoder" signal for
    /// the orchestrator's inbound-stall watchdog + bytes/s telemetry. `None`
    /// when the API is unreachable / non-200 / the path is absent / the field
    /// is missing or negative.
    pub async fn inbound_bytes(&self, name: &str) -> Option<u64> {
        let body = http_get(self.api_port, &format!("/v3/paths/get/{name}")).await?;
        let data = serde_json::from_slice::<serde_json::Value>(&body).ok()?;
        let value = data.get("bytesReceived")?.as_u64()?;
        Some(value)
    }
}

/// Drain one of mediamtx's output pipes (it logs to stdout; stderr carries
/// startup failures) so neither 64 KB pipe can fill and block mediamtx's next
/// write, which would freeze the pipeline while the process still looks alive.
/// mediamtx runs at `logLevel: warn`, so every line it emits is a warning or an
/// error worth seeing (a slow reader discarding frames, a refused publisher)
/// and the volume is low; they are logged at warn.
async fn drain_mediamtx_log<R: tokio::io::AsyncRead + Unpin>(stream: R) {
    let mut lines = crate::stderr_drain::BoundedLines::new(stream);
    while let Some(line) = lines.next_line().await {
        let text = line.trim_end();
        if !text.is_empty() {
            tracing::warn!(line = %text, "mediamtx_log");
        }
    }
}
