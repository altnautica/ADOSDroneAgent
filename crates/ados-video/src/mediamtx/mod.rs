//! `mediamtx` subprocess manager: config generation, start/readiness, and the
//! per-path API queries the orchestrator's watchdog reads.
//!
//! mediamtx is the local RTSP/WebRTC/HLS server the encoder publishes into and
//! the browser pulls WHEP from, through the agent's credentialed LAN front (its
//! HTTP listeners bind loopback). This module owns:
//! - a pure [`mediamtx_config_yaml`] that renders the exact `mediamtx.yml` the
//!   predecessor generated (ports, WebRTC ICE binding, STUN list, the `main`
//!   publisher path);
//! - a [`MediamtxManager`] that spawns `mediamtx <config>` through
//!   [`crate::process::ManagedProcess`] (the setsid/killpg owner — no second
//!   spawner), gates startup on the RTSP listener actually accepting, and
//!   answers the two watchdog queries: per-path `ready` and per-path
//!   `bytesReceived`.
//!
//! The API queries hit mediamtx's control API on 127.0.0.1:9997 by PATH NAME
//! (`/v3/paths/get/<name>`), never by list index: the path list also carries
//! the WHEP consumer path, so `items[0]` can be an unrelated never-ready path.
//!
//! The workspace has no async HTTP client (`ureq` is blocking), so this module
//! carries a ~40-line async HTTP/1.1 GET over a raw `tokio` TCP stream. It only
//! ever talks to loopback, so HTTP/1.0-style `Connection: close` + read-to-EOF
//! is enough; no chunked-transfer or keep-alive handling is needed.

use std::time::Duration;

use serde::Serialize;

mod config;
mod manager;
mod net;

pub use config::{mediamtx_config_yaml, ConfigParams};
pub use manager::MediamtxManager;
pub use net::{detect_lan_ips, physical_lan_interfaces, wait_for_tcp_port};

/// mediamtx control-API port.
pub const DEFAULT_API_PORT: u16 = 9997;
/// RTSP listener port (the encoder publishes `rtsp://localhost:8554/main`).
pub const DEFAULT_RTSP_PORT: u16 = 8554;
/// WebRTC (WHEP) listener port.
pub const DEFAULT_WEBRTC_PORT: u16 = 8889;
/// HLS-LL listener port (fallback when WebRTC is blocked).
pub const DEFAULT_HLS_PORT: u16 = 8888;
/// The single ICE host port WebRTC UDP+TCP candidates are pinned to.
const WEBRTC_LOCAL_ICE_PORT: u16 = 8189;

/// The path name the air-side encoder publishes to. The readiness + inbound
/// watchdog look the path up by this name rather than assuming list index 0.
pub const MAIN_PATH: &str = "main";

/// RTSP-bind readiness gate window. A cold-boot Pi 4B binds the RTSP listener
/// in ~150-300 ms, but first-boot-after-install load has pushed it past 1 s;
/// the encoder then lost the publish race and died with "failed to open output
/// file". Gate the encoder spawn on the listener actually accepting instead of
/// a fixed sleep.
pub const RTSP_BIND_TIMEOUT: Duration = Duration::from_secs(10);
const RTSP_BIND_PROBE_INTERVAL: Duration = Duration::from_millis(50);

/// API query budget: 2 s total, 0.5 s connect (mirrors the httpx Timeout).
const API_TIMEOUT: Duration = Duration::from_secs(2);
const API_CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

/// mediamtx playback-server port. The playback server is the read side of the
/// native recorder: `GET /list?path=<p>&start=&end=` enumerates the recorded
/// timespans and `GET /get?path=<p>&start=<t>&duration=<n>` returns fMP4 a
/// browser `<video>` plays directly. It is what makes "export the 30 s around
/// this event" a query rather than a ring buffer.
pub const DEFAULT_PLAYBACK_PORT: u16 = 9996;

/// Root of the on-disk recording tree. Segments land under
/// `<root>/<path>/<timestamp>.mp4` via [`record_path_template`].
pub const RECORDINGS_ROOT: &str = "/var/ados/recordings";

/// STUN servers for WebRTC ICE NAT traversal, used ONLY on the cloud-relay
/// profile. All five are free + unlimited; more candidates means a higher
/// chance of a working ICE pair on cellular carriers and restricted NATs.
///
/// They are deliberately ABSENT on the LAN/direct profiles. A LAN WHEP session
/// connects on host candidates, so every STUN entry there is pure handshake
/// latency gated by [`STUN_GATHER_TIMEOUT`] before the first frame can flow.
const STUN_SERVERS: &[&str] = &[
    "stun:stun.l.google.com:19302",
    "stun:stun1.l.google.com:19302",
    "stun:stun2.l.google.com:19302",
    "stun:stun.cloudflare.com:3478",
    "stun:global.stun.twilio.com:3478",
];

/// Per-reader write queue depth, in RTP packets (NOT bytes).
///
/// When a reader's queue overflows mediamtx logs "reader is too slow,
/// discarding N frames" and that reader's stream is corrupted from then on —
/// the canonical browser-WebRTC "freeze on last frame, refresh restores"
/// symptom. Upstream's default is 512 and upstream's guidance is to raise it
/// only on an observed overflow. The GROUND side observed exactly that, and its
/// own config comment recorded 512 causing reader eviction and the freeze.
///
/// Headroom math: ~30 fps and worst-case ~50 RTP packets per 1280x720 H.264
/// frame is ~1500 packets/s, so 4096 holds ~2.7 s. That survives a routine
/// Chrome GC pause, a paint frame, a tab focus change and a brief SBC swap-in
/// stall — every one of which the 512 ceiling turned into reader eviction.
///
/// The AIR side serves the same browser WHEP readers over the same code path,
/// so it gets the same depth: the divergence (absent on air ⇒ upstream 512) was
/// the bug, not a deliberate air-side choice. Cost is memory per active reader
/// (~5 MB), not delay — an unoccupied queue adds nothing.
const WRITE_QUEUE_SIZE: u32 = 4096;

/// Max UDP payload, matching mediamtx's own default. Pinned explicitly on both
/// profiles rather than inherited on one and hardcoded to a different value
/// (1472) on the other.
const UDP_MAX_PAYLOAD_SIZE: u32 = 1452;

/// RTSP read/write timeouts.
///
/// PINNED AT UPSTREAM'S DEFAULT (10s) ON PURPOSE. Tightening this to 5 s looks
/// like free failure detection and is not: the ground config's own comment
/// records that a 5 s ceiling under low-RAM swap pressure caught system
/// stutters the 10 s default absorbs, and produced a DETERMINISTIC 120 s
/// publisher-eviction cycle on Pi 4B 1 GB boards. The 10 s window is what gives
/// the kernel room to page mediamtx's working set back in without tearing the
/// publisher session down. A wedged publisher is caught by the delta-counter
/// watchdogs (which assert bytes actually move), not by shortening this.
const RTSP_IO_TIMEOUT: &str = "10s";

/// ICE gather ceiling, where STUN is used at all. Upstream defaults to 5 s,
/// which is 5 s of pre-first-frame latency on any session that would have
/// connected on host candidates anyway.
const STUN_GATHER_TIMEOUT: &str = "2s";

/// WebRTC handshake ceiling. Upstream's default is 10 s; the 15 s both profiles
/// carried bought nothing over it.
const WEBRTC_HANDSHAKE_TIMEOUT: &str = "10s";

/// LL-HLS part duration. A player buffers roughly three parts, so this value IS
/// the LL-HLS latency floor (~600 ms at 200 ms). Pinned rather than inherited so
/// an upstream default change cannot silently move the floor.
const HLS_PART_DURATION: &str = "200ms";

/// fMP4 part duration, and therefore the recording RPO: a power cut, SIGKILL or
/// full disk loses at most this much video. Each part is independently flushed,
/// which is what makes the native recorder crash-safe by construction — unlike
/// a `-movflags +faststart` MP4, whose `moov` atom is written only on a clean
/// exit and which therefore yields a ZERO-recoverable file on an unclean one.
const RECORD_PART_DURATION: &str = "200ms";

/// Max bytes per fMP4 part, matching upstream's default.
const RECORD_MAX_PART_SIZE: &str = "50M";

/// One segment file per minute. This bounds the blast radius of any single
/// corrupt file and is the granularity the playback server lists.
pub const RECORD_SEGMENT_DURATION: &str = "60s";

/// Time-based retention. NOT sufficient on its own: a high-bitrate day fills
/// the volume long before 24 h elapse and recording then stops dead, so the
/// space-based reclaim in the supervisor janitor is the companion guard.
const RECORD_DELETE_AFTER: &str = "24h";

/// The `recordPath` template. `%path` is the mediamtx path name, so each leg
/// records into its own subdirectory; the rest is the segment start timestamp
/// down to microseconds (`%f`), which is what keeps two segments opened inside
/// the same second from colliding.
pub fn record_path_template(root: &str) -> String {
    format!("{root}/%path/%Y-%m-%d_%H-%M-%S-%f")
}

/// Which node this config is being rendered for.
///
/// This is the one generator for every profile. The divergence it replaces was
/// two independent generators (an air-side Rust renderer and a ground-side
/// Python one) that had drifted apart on `writeQueueSize`, `udpMaxPayloadSize`
/// and the WebRTC timeouts — with no gate able to see it. Everything that is
/// genuinely shared is rendered from the constants above for every profile; a
/// `match` on this enum is the ONLY place a value may legitimately differ, and
/// each arm has to say why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediamtxProfile {
    /// The drone. Encoder publishes locally; readers are browser WHEP clients
    /// on the LAN plus the wfb radio tap.
    Air,
    /// The ground station. The media server's native `udp+rtp` source takes the
    /// radio-decoded stream; readers are browser WHEP clients on the LAN.
    Ground,
    /// Reached through a cloud relay rather than the LAN, so ICE actually needs
    /// STUN and a TCP candidate is a legitimate last resort.
    CloudRelay,
}

impl MediamtxProfile {
    /// The ICE TCP listener address.
    ///
    /// Empty (upstream's own default) on the LAN/direct profiles: upstream
    /// disables it by default because "TCP is less efficient than UDP and
    /// introduces a progressive delay when network is congested". On a lossy
    /// air link that is the difference between FEC-tolerant packet loss and
    /// head-of-line-blocked latency that never recovers once a TCP candidate
    /// wins the pair. Only the cloud-relay profile, which may face a UDP-hostile
    /// network, keeps it.
    fn webrtc_local_tcp_address(self) -> String {
        match self {
            Self::Air | Self::Ground => String::new(),
            Self::CloudRelay => format!(":{WEBRTC_LOCAL_ICE_PORT}"),
        }
    }

    /// STUN servers: none on a LAN session that connects on host candidates,
    /// the full list when traversing a relay.
    fn ice_servers(self) -> Vec<IceServer> {
        match self {
            Self::Air | Self::Ground => Vec::new(),
            Self::CloudRelay => STUN_SERVERS
                .iter()
                .map(|u| IceServer {
                    url: (*u).to_string(),
                })
                .collect(),
        }
    }

    /// The HLS variant.
    ///
    /// `lowLatency` everywhere EXCEPT the ground station, which is pinned to
    /// `mpegts` for measured reasons recorded on that rig: `lowLatency` had
    /// HLS.js CAN-BLOCK-RELOAD requests saturate the HTTP/1.1 6-connection
    /// per-origin pool and freeze the player while mediamtx kept publishing,
    /// and `fmp4` served the playlist and init segment but 404'd every media
    /// segment. `mpegts` has higher latency and actually serves segments, and
    /// HLS on the ground is only ever the freeze-resistant fallback behind
    /// WHEP. This is a genuine per-profile difference, not drift.
    fn hls_variant(self) -> &'static str {
        match self {
            Self::Air | Self::CloudRelay => "lowLatency",
            Self::Ground => "mpegts",
        }
    }
}

/// One `{"url": "stun:..."}` entry of the `webrtcICEServers2` list.
#[derive(Debug, Serialize)]
struct IceServer {
    url: String,
}

/// Recording knobs resolved from `video.recording` in the agent config.
///
/// This is what finally makes that config block live: before the native
/// recorder it was read by nothing but a snapshots-subdirectory derivation.
#[derive(Debug, Clone)]
pub struct RecordingParams {
    /// Whether mediamtx records continuously. With this on, the on-disk segment
    /// set IS the pre-roll ring — an arbitrary window before any event is
    /// extracted after the fact through the playback server, so no in-RAM ring
    /// buffer is needed at all.
    pub enabled: bool,
    /// Root of the recording tree.
    pub root: String,
    /// Per-segment duration.
    pub segment_duration: String,
    /// Time-based retention window.
    pub delete_after: String,
    /// Optional `runOnRecordSegmentComplete` hook. mediamtx passes
    /// `MTX_SEGMENT_PATH` and `MTX_SEGMENT_DURATION` in the environment; the
    /// hook is how space-based reclaim is driven off a real segment-close event
    /// rather than a poll.
    pub on_segment_complete: Option<String>,
}

impl Default for RecordingParams {
    fn default() -> Self {
        Self {
            enabled: false,
            root: RECORDINGS_ROOT.to_string(),
            segment_duration: RECORD_SEGMENT_DURATION.to_string(),
            delete_after: RECORD_DELETE_AFTER.to_string(),
            on_segment_complete: None,
        }
    }
}

#[cfg(test)]
mod tests;
