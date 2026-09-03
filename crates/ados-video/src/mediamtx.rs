//! `mediamtx` subprocess manager: config generation, start/readiness, and the
//! per-path API queries the orchestrator's watchdog reads.
//!
//! mediamtx is the local RTSP/WebRTC/HLS server the encoder publishes into and
//! the browser pulls WHEP from. This module owns:
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

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::process::ManagedProcess;

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
    /// The ground station. The ffmpeg ingest publishes the radio-decoded stream;
    /// readers are browser WHEP clients on the LAN.
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

/// A path config entry. `sourceOnDemand` is omitted for the publisher source
/// (it is only valid for non-publisher sources), so it is an `Option` skipped
/// when `None`.
#[derive(Debug, Serialize)]
struct PathConfig {
    source: String,
    #[serde(rename = "sourceOnDemand", skip_serializing_if = "Option::is_none")]
    source_on_demand: Option<bool>,
}

/// The `pathDefaults:` block — settings applied to every path unless a specific
/// path overrides them. The record keys and `useAbsoluteTimestamp` live here
/// (they are per-path in mediamtx, NOT global).
#[derive(Debug, Serialize)]
struct PathDefaults {
    /// Route the ORIGINAL absolute timestamps of RTSP/WebRTC frames instead of
    /// replacing them. Upstream defaults this to false, which means mediamtx
    /// overwrites each frame's timestamp with the current time and destroys the
    /// capture-time information — exactly the field a frame-to-pose or
    /// glass-to-glass measurement needs. There is no reason to prefer the
    /// rewritten value on either profile.
    #[serde(rename = "useAbsoluteTimestamp")]
    use_absolute_timestamp: bool,
    record: bool,
    #[serde(rename = "recordPath")]
    record_path: String,
    #[serde(rename = "recordFormat")]
    record_format: String,
    #[serde(rename = "recordPartDuration")]
    record_part_duration: String,
    #[serde(rename = "recordMaxPartSize")]
    record_max_part_size: String,
    #[serde(rename = "recordSegmentDuration")]
    record_segment_duration: String,
    #[serde(rename = "recordDeleteAfter")]
    record_delete_after: String,
    #[serde(
        rename = "runOnRecordSegmentComplete",
        skip_serializing_if = "Option::is_none"
    )]
    run_on_record_segment_complete: Option<String>,
}

/// The full `mediamtx.yml` document. Field names use the mediamtx camelCase
/// keys verbatim via `rename`. Every key here is checked against upstream's
/// reference `mediamtx.yml`: mediamtx REJECTS an unrecognised key outright, so
/// a typo does not degrade — it leaves the node with no media server at all.
#[derive(Debug, Serialize)]
struct MediamtxConfig {
    #[serde(rename = "logLevel")]
    log_level: String,
    api: bool,
    #[serde(rename = "apiAddress")]
    api_address: String,
    #[serde(rename = "readTimeout")]
    read_timeout: String,
    #[serde(rename = "writeTimeout")]
    write_timeout: String,
    #[serde(rename = "writeQueueSize")]
    write_queue_size: u32,
    #[serde(rename = "udpMaxPayloadSize")]
    udp_max_payload_size: u32,
    /// Serve recorded segments back over HTTP. This is the read half of the
    /// native recorder and the whole reason no clip-export machinery has to be
    /// built: mark-in/mark-out becomes a `/get` query against segments that are
    /// already on disk.
    playback: bool,
    #[serde(rename = "playbackAddress")]
    playback_address: String,
    rtsp: bool,
    #[serde(rename = "rtspAddress")]
    rtsp_address: String,
    // mediamtx's `rtsp: true` default also opens UDP RTP/RTCP listeners
    // (`rtpAddress`/`rtcpAddress`, mediamtx's own defaults `:8000`/`:8001`)
    // for the "udp" RTSP transport — unwanted here (the encoder publishes
    // over the TCP RTSP listener above; no client on this box reads RTSP at
    // all, let alone over UDP) and a real collision: `:8000`-`:8003` is
    // ados-radio's wfb_tx control-port range (`TX_CMD_PORT_BASE`,
    // collision-free by construction only if nothing else claims it).
    // Restricting to `tcp` stops mediamtx from binding those UDP ports at
    // all, so wfb_tx's control sockets never race it for 8000/8001.
    #[serde(rename = "rtspTransports")]
    rtsp_transports: Vec<String>,
    webrtc: bool,
    #[serde(rename = "webrtcAddress")]
    webrtc_address: String,
    #[serde(rename = "webrtcAllowOrigin")]
    webrtc_allow_origin: String,
    #[serde(rename = "webrtcIPsFromInterfaces")]
    webrtc_ips_from_interfaces: bool,
    #[serde(rename = "webrtcIPsFromInterfacesList")]
    webrtc_ips_from_interfaces_list: Vec<String>,
    #[serde(rename = "webrtcHandshakeTimeout")]
    webrtc_handshake_timeout: String,
    #[serde(rename = "webrtcSTUNGatherTimeout")]
    webrtc_stun_gather_timeout: String,
    #[serde(rename = "webrtcLocalUDPAddress")]
    webrtc_local_udp_address: String,
    #[serde(rename = "webrtcLocalTCPAddress")]
    webrtc_local_tcp_address: String,
    #[serde(rename = "webrtcICEServers2")]
    webrtc_ice_servers2: Vec<IceServer>,
    hls: bool,
    #[serde(rename = "hlsAddress")]
    hls_address: String,
    #[serde(rename = "hlsAlwaysRemux")]
    hls_always_remux: bool,
    #[serde(rename = "hlsVariant")]
    hls_variant: String,
    #[serde(rename = "hlsSegmentCount")]
    hls_segment_count: u32,
    #[serde(rename = "hlsSegmentDuration")]
    hls_segment_duration: String,
    #[serde(rename = "hlsPartDuration")]
    hls_part_duration: String,
    #[serde(rename = "hlsAllowOrigin")]
    hls_allow_origin: String,
    #[serde(
        rename = "webrtcAdditionalHosts",
        skip_serializing_if = "Option::is_none"
    )]
    webrtc_additional_hosts: Option<Vec<String>>,
    #[serde(rename = "pathDefaults")]
    path_defaults: PathDefaults,
    paths: std::collections::BTreeMap<String, PathConfig>,
}

/// Inputs to the pure config renderer.
pub struct ConfigParams<'a> {
    /// Which node this is for. The only sanctioned source of value divergence.
    pub profile: MediamtxProfile,
    pub api_port: u16,
    pub rtsp_port: u16,
    pub webrtc_port: u16,
    pub hls_port: u16,
    pub playback_port: u16,
    /// Detected LAN IPv4 addresses, advertised as `webrtcAdditionalHosts`.
    /// Empty → the additional-hosts key is omitted.
    pub lan_ips: &'a [String],
    /// Stream name → source. `"main" -> "publisher"` for a locally-published
    /// leg. A non-`"publisher"` source gets `sourceOnDemand: true`.
    pub streams: &'a [(String, String)],
    /// Recording knobs, from `video.recording`.
    pub recording: RecordingParams,
}

impl<'a> ConfigParams<'a> {
    /// Default ports for `profile`, no LAN IPs, no recording. Callers override
    /// the fields they care about; this keeps every construction site from
    /// having to restate the six port defaults.
    pub fn new(
        profile: MediamtxProfile,
        lan_ips: &'a [String],
        streams: &'a [(String, String)],
    ) -> Self {
        Self {
            profile,
            api_port: DEFAULT_API_PORT,
            rtsp_port: DEFAULT_RTSP_PORT,
            webrtc_port: DEFAULT_WEBRTC_PORT,
            hls_port: DEFAULT_HLS_PORT,
            playback_port: DEFAULT_PLAYBACK_PORT,
            lan_ips,
            streams,
            recording: RecordingParams::default(),
        }
    }
}

/// Render the `mediamtx.yml` document for the given parameters.
///
/// The WebRTC media sockets bind to all interfaces (`:8189`) and ICE host
/// candidates are gathered from the real physical interfaces (re-read by Pion
/// per session), so the media path follows an interface/IP change
/// (ethernet->WiFi failover, DHCP) instead of being pinned to one boot-time IP.
/// `webrtcAdditionalHosts` still advertises the detected outbound IP as a hint.
pub fn mediamtx_config_yaml(params: &ConfigParams) -> String {
    // Bind the WebRTC media sockets to ALL interfaces (":8189") so the media
    // path is never pinned to a single IP that can disappear (ethernet->WiFi
    // failover, DHCP change, multi-homing). ICE host candidates are gathered
    // from the real physical interfaces, re-read by Pion per session, so
    // whatever interface is up at connect time is advertised.
    let phys_ifaces = physical_lan_interfaces();

    let mut paths = std::collections::BTreeMap::new();
    for (name, source) in params.streams {
        let source_on_demand = if source == "publisher" {
            None
        } else {
            Some(true)
        };
        paths.insert(
            name.clone(),
            PathConfig {
                source: source.clone(),
                source_on_demand,
            },
        );
    }

    let rec = &params.recording;
    let config = MediamtxConfig {
        log_level: "warn".into(),
        api: true,
        api_address: format!(":{}", params.api_port),
        read_timeout: RTSP_IO_TIMEOUT.into(),
        write_timeout: RTSP_IO_TIMEOUT.into(),
        write_queue_size: WRITE_QUEUE_SIZE,
        udp_max_payload_size: UDP_MAX_PAYLOAD_SIZE,
        // The playback server is always listening, even when `record` is off:
        // it is the read surface for whatever segments are already on disk, and
        // an operator who just turned recording off still wants to export the
        // flight they recorded a minute ago.
        playback: true,
        playback_address: format!(":{}", params.playback_port),
        rtsp: true,
        rtsp_address: format!(":{}", params.rtsp_port),
        rtsp_transports: vec!["tcp".into()],
        webrtc: true,
        webrtc_address: format!(":{}", params.webrtc_port),
        webrtc_allow_origin: "*".into(),
        webrtc_ips_from_interfaces: true,
        webrtc_ips_from_interfaces_list: phys_ifaces,
        webrtc_handshake_timeout: WEBRTC_HANDSHAKE_TIMEOUT.into(),
        webrtc_stun_gather_timeout: STUN_GATHER_TIMEOUT.into(),
        webrtc_local_udp_address: format!(":{WEBRTC_LOCAL_ICE_PORT}"),
        webrtc_local_tcp_address: params.profile.webrtc_local_tcp_address(),
        webrtc_ice_servers2: params.profile.ice_servers(),
        hls: true,
        hls_address: format!(":{}", params.hls_port),
        // Remux HLS only while something is actually watching it.
        //
        // With this on, mediamtx keeps a low-latency muxer segmenting the
        // stream at 1s even when no HLS client exists. Measured on a ground
        // station: the muxer sat attached as a reader with `bytesSent=0` --
        // it had never delivered a single byte -- while the box ran at load
        // 4.94 on four cores and the operator's video froze.
        //
        // HLS itself stays available (the on-box dashboard's video panel uses
        // it); mediamtx starts the muxer on the first request instead. The
        // cost is a slightly slower first HLS frame, paid only by whoever asks
        // for it, rather than continuous CPU spent on nobody.
        hls_always_remux: false,
        hls_variant: params.profile.hls_variant().into(),
        hls_segment_count: 7,
        hls_segment_duration: "1s".into(),
        hls_part_duration: HLS_PART_DURATION.into(),
        hls_allow_origin: "*".into(),
        webrtc_additional_hosts: if params.lan_ips.is_empty() {
            None
        } else {
            Some(params.lan_ips.to_vec())
        },
        path_defaults: PathDefaults {
            use_absolute_timestamp: true,
            record: rec.enabled,
            record_path: record_path_template(&rec.root),
            record_format: "fmp4".into(),
            record_part_duration: RECORD_PART_DURATION.into(),
            record_max_part_size: RECORD_MAX_PART_SIZE.into(),
            record_segment_duration: rec.segment_duration.clone(),
            record_delete_after: rec.delete_after.clone(),
            run_on_record_segment_complete: rec.on_segment_complete.clone(),
        },
        paths,
    };

    serde_norway::to_string(&config).expect("mediamtx config serializes")
}

/// Discover the SBC's outbound LAN IPv4 by opening a UDP socket toward a public
/// address (no packet is sent — a UDP connect is just a routing-table lookup)
/// and reading the bound local address. mediamtx ICE auto-discovery sometimes
/// only finds 127.0.0.1 on a bench rig, so this is forced into the config as a
/// WebRTC host candidate. Filters loopback + link-local. Best-effort: returns
/// empty on any error.
pub fn detect_lan_ips() -> Vec<String> {
    let mut ips = Vec::new();
    if let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if sock.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = sock.local_addr() {
                let ip = addr.ip().to_string();
                if ip != "127.0.0.1" && !ip.starts_with("169.254.") {
                    ips.push(ip);
                }
            }
        }
    }
    ips
}

/// Names of physical wired/WiFi interfaces (`e*`/`en*`/`eth*`/`end*`, `w*`/
/// `wl*`/`wlan*`), excluding loopback, virtual, container, and mesh interfaces,
/// read from `/sys/class/net`. Used to scope WebRTC ICE host-candidate
/// gathering to real reachable networks so the offer never carries loopback /
/// IPv6 link-local / docker / mesh candidates that just fail their checks.
/// mediamtx (Pion) re-reads the addresses of these interfaces per WebRTC
/// session, so a node that moves from ethernet to WiFi advertises whichever
/// interface is up. Best-effort: returns empty when `/sys/class/net` is
/// unreadable (mediamtx then falls back to all interfaces).
pub fn physical_lan_interfaces() -> Vec<String> {
    const SKIP: &[&str] = &[
        "lo", "docker", "veth", "br-", "bat", "tap", "tun", "wg", "virbr", "vmnet",
    ];
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/sys/class/net") {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if SKIP.iter().any(|p| name.starts_with(p)) {
                    continue;
                }
                if name.starts_with('e') || name.starts_with('w') {
                    names.push(name.to_string());
                }
            }
        }
    }
    names.sort();
    names
}

/// Async TCP-connect probe: poll `(host, port)` at [`RTSP_BIND_PROBE_INTERVAL`]
/// until a connect succeeds or `timeout` elapses. Each probe uses a short
/// connect timeout so a stalled stack does not hold the loop. Used to gate the
/// encoder spawn on the mediamtx RTSP listener actually accepting.
pub async fn wait_for_tcp_port(host: &str, port: u16, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let connect = TcpStream::connect((host, port));
        if let Ok(Ok(stream)) = tokio::time::timeout(API_CONNECT_TIMEOUT, connect).await {
            drop(stream);
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(RTSP_BIND_PROBE_INTERVAL).await;
    }
}

/// Minimal async HTTP/1.1 GET to 127.0.0.1:`port``path`. Returns the response
/// body bytes on a 200, `None` on connection-refused / timeout / non-200 /
/// malformed response. Loopback-only, so `Connection: close` + read-to-EOF is
/// sufficient (no chunked / keep-alive handling).
async fn http_get(port: u16, path: &str) -> Option<Vec<u8>> {
    let connect = TcpStream::connect(("127.0.0.1", port));
    let mut stream = tokio::time::timeout(API_CONNECT_TIMEOUT, connect)
        .await
        .ok()?
        .ok()?;

    let request =
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");

    let exchange = async {
        stream.write_all(request.as_bytes()).await.ok()?;
        stream.flush().await.ok()?;
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.ok()?;
        Some(buf)
    };
    let raw = tokio::time::timeout(API_TIMEOUT, exchange).await.ok()??;

    // Split status line + headers from the body on the first CRLFCRLF.
    let sep = find_subslice(&raw, b"\r\n\r\n")?;
    let head = &raw[..sep];
    let body = &raw[sep + 4..];

    // The status line is the first line of the head: "HTTP/1.1 200 OK".
    let status_line_end = find_subslice(head, b"\r\n").unwrap_or(head.len());
    let status_line = std::str::from_utf8(&head[..status_line_end]).ok()?;
    let code: u16 = status_line.split_whitespace().nth(1)?.parse().ok()?;
    if code != 200 {
        return None;
    }
    Some(body.to_vec())
}

/// Find the first index of `needle` in `haystack`.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

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
        let mut p = ManagedProcess::spawn("mediamtx", "mediamtx", &[config])?;
        // Drain stderr in the background to prevent the 64KB pipe buffer from
        // filling and blocking mediamtx's next write (which freezes the whole
        // video pipeline while the process still looks alive).
        if let Some(stderr) = p.take_stderr() {
            tokio::spawn(drain_mediamtx_stderr(stderr));
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

/// Drain mediamtx stderr to prevent a pipe-buffer deadlock. mediamtx logs
/// WebRTC connection events + RTSP sessions here; an undrained pipe fills at
/// 64KB and blocks mediamtx's next write, freezing the pipeline while the
/// process still looks alive. Logged at debug — mediamtx is configured
/// `logLevel: warn`, so this is low-volume.
async fn drain_mediamtx_stderr(stderr: tokio::process::ChildStderr) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let text = line.trim_end();
        if !text.is_empty() {
            tracing::debug!(line = %text, "mediamtx_stderr");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::collections::BTreeMap;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn default_params<'a>(
        lan_ips: &'a [String],
        streams: &'a [(String, String)],
    ) -> ConfigParams<'a> {
        ConfigParams::new(MediamtxProfile::Air, lan_ips, streams)
    }

    // --- config field + value parity ----------------------------------

    #[test]
    fn config_has_exact_ports_and_main_path() {
        let lan = vec!["192.168.1.115".to_string()];
        let streams = vec![("main".to_string(), "publisher".to_string())];
        let yaml = mediamtx_config_yaml(&default_params(&lan, &streams));

        // Parse back and assert the structure rather than string-matching the
        // whole document (mediamtx, not the test, defines acceptable YAML).
        let v: Value = serde_norway::from_str(&yaml).unwrap();

        assert_eq!(v["apiAddress"], ":9997");
        assert_eq!(v["rtspAddress"], ":8554");
        assert_eq!(v["webrtcAddress"], ":8889");
        assert_eq!(v["hlsAddress"], ":8888");
        assert_eq!(v["api"], true);
        assert_eq!(v["rtsp"], true);
        // UDP RTSP transport disabled: mediamtx's own default `rtpAddress`/
        // `rtcpAddress` (:8000/:8001) collide with ados-radio's wfb_tx
        // control-port range.
        assert_eq!(v["rtspTransports"], Value::from(vec!["tcp"]));
        assert_eq!(v["webrtc"], true);
        assert_eq!(v["hls"], true);
        assert_eq!(v["logLevel"], "warn");
        assert_eq!(v["webrtcAllowOrigin"], "*");
        // Upstream's own default. The 15s both profiles carried bought nothing.
        assert_eq!(v["webrtcHandshakeTimeout"], "10s");
        assert_eq!(v["webrtcSTUNGatherTimeout"], "2s");
        // Pinned at upstream's default, NOT tightened to 5s: the ground rig
        // measured a 5s ceiling producing a deterministic 120s publisher
        // eviction cycle on Pi 4B 1 GB boards under swap pressure.
        assert_eq!(v["readTimeout"], "10s");
        assert_eq!(v["writeTimeout"], "10s");
        // Burst tolerance, raised off upstream's 512 on BOTH profiles: the
        // ground observed 512 evicting browser WHEP readers, and the air side
        // serves the same readers.
        assert_eq!(v["writeQueueSize"], 4096);
        assert_eq!(v["udpMaxPayloadSize"], 1452);
        // The playback server is the read side of the native recorder, and it
        // listens even when `record` is off so already-captured segments stay
        // exportable.
        assert_eq!(v["playback"], true);
        assert_eq!(v["playbackAddress"], ":9996");

        // WebRTC media binds all interfaces (:8189); ICE host candidates are
        // gathered from the physical interfaces per session so the media path
        // follows an interface/IP change. The interface list is read from
        // /sys/class/net at runtime, so assert only that it is a sequence (its
        // contents depend on the host running the test).
        assert_eq!(v["webrtcIPsFromInterfaces"], true);
        assert!(v["webrtcIPsFromInterfacesList"].as_array().is_some());
        assert_eq!(v["webrtcLocalUDPAddress"], ":8189");
        // Empty on the LAN/direct profile (upstream's own default): a winning
        // TCP ICE candidate on a lossy air link turns FEC-tolerant loss into
        // head-of-line-blocked latency that never recovers.
        assert_eq!(v["webrtcLocalTCPAddress"], "");
        assert_eq!(
            v["webrtcAdditionalHosts"],
            Value::from(vec!["192.168.1.115"])
        );

        // HLS low-latency, 7 segs x 1s, always remux.
        assert_eq!(v["hlsAlwaysRemux"], false);
        assert_eq!(v["hlsVariant"], "lowLatency");
        assert_eq!(v["hlsSegmentCount"], 7);
        assert_eq!(v["hlsSegmentDuration"], "1s");
        assert_eq!(v["hlsAllowOrigin"], "*");
        // The LL-HLS latency floor, pinned rather than inherited.
        assert_eq!(v["hlsPartDuration"], "200ms");

        // The `main` publisher path: source=publisher, no sourceOnDemand.
        assert_eq!(v["paths"]["main"]["source"], "publisher");
        assert!(v["paths"]["main"].get("sourceOnDemand").is_none());
    }

    /// STUN is cloud-relay-only. On a LAN session that connects on host
    /// candidates every entry is pure pre-first-frame handshake latency.
    #[test]
    fn lan_profiles_carry_no_stun_and_cloud_relay_carries_all_five() {
        let lan: Vec<String> = vec![];
        let streams = vec![("main".to_string(), "publisher".to_string())];

        for profile in [MediamtxProfile::Air, MediamtxProfile::Ground] {
            let yaml = mediamtx_config_yaml(&ConfigParams::new(profile, &lan, &streams));
            let v: Value = serde_norway::from_str(&yaml).unwrap();
            assert_eq!(
                v["webrtcICEServers2"].as_array().unwrap().len(),
                0,
                "{profile:?} is a LAN/direct profile and must gather no STUN"
            );
            assert_eq!(
                v["webrtcLocalTCPAddress"], "",
                "{profile:?} must not offer a TCP ICE candidate"
            );
        }

        let yaml = mediamtx_config_yaml(&ConfigParams::new(
            MediamtxProfile::CloudRelay,
            &lan,
            &streams,
        ));
        let v: Value = serde_norway::from_str(&yaml).unwrap();
        let urls: Vec<&str> = v["webrtcICEServers2"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["url"].as_str().unwrap())
            .collect();
        assert_eq!(
            urls,
            vec![
                "stun:stun.l.google.com:19302",
                "stun:stun1.l.google.com:19302",
                "stun:stun2.l.google.com:19302",
                "stun:stun.cloudflare.com:3478",
                "stun:global.stun.twilio.com:3478",
            ]
        );
        // A relay may face a UDP-hostile network, so TCP is a legitimate last
        // resort there and only there.
        assert_eq!(v["webrtcLocalTCPAddress"], ":8189");
    }

    /// THE PARITY GATE.
    ///
    /// The air and ground mediamtx configs were rendered by two independent
    /// generators that had silently drifted apart — `writeQueueSize` absent on
    /// air and 4096 on ground, `udpMaxPayloadSize` absent on air and 1472 on
    /// ground — and nothing could see it. Every key below is genuinely shared:
    /// both profiles serve the same browser WHEP readers off the same code
    /// path, so a difference is drift, not a decision.
    ///
    /// The two keys that ARE allowed to differ (`hlsVariant`,
    /// `webrtcLocalTCPAddress` / `webrtcICEServers2`) are asserted to differ
    /// elsewhere, so this list staying exhaustive is what makes the gate real:
    /// a new key added to one arm of the profile `match` without a stated
    /// reason fails here.
    #[test]
    fn air_and_ground_agree_on_every_shared_key() {
        let lan = vec!["192.168.1.50".to_string()];
        let streams = vec![("main".to_string(), "publisher".to_string())];

        let air: Value = serde_norway::from_str(&mediamtx_config_yaml(&ConfigParams::new(
            MediamtxProfile::Air,
            &lan,
            &streams,
        )))
        .unwrap();
        let ground: Value = serde_norway::from_str(&mediamtx_config_yaml(&ConfigParams::new(
            MediamtxProfile::Ground,
            &lan,
            &streams,
        )))
        .unwrap();

        const SHARED_KEYS: &[&str] = &[
            "logLevel",
            "api",
            "apiAddress",
            "readTimeout",
            "writeTimeout",
            "writeQueueSize",
            "udpMaxPayloadSize",
            "playback",
            "playbackAddress",
            "rtsp",
            "rtspAddress",
            "rtspTransports",
            "webrtc",
            "webrtcAddress",
            "webrtcAllowOrigin",
            "webrtcIPsFromInterfaces",
            "webrtcIPsFromInterfacesList",
            "webrtcHandshakeTimeout",
            "webrtcSTUNGatherTimeout",
            "webrtcLocalUDPAddress",
            "webrtcAdditionalHosts",
            "hls",
            "hlsAddress",
            "hlsAlwaysRemux",
            "hlsSegmentCount",
            "hlsSegmentDuration",
            "hlsPartDuration",
            "hlsAllowOrigin",
            "pathDefaults",
            "paths",
        ];

        for key in SHARED_KEYS {
            assert_eq!(
                air[key], ground[key],
                "air and ground diverge on the shared key `{key}`: \
                 air={:?} ground={:?}. Either it is genuinely per-profile \
                 (put it behind a MediamtxProfile method with a stated reason \
                 and drop it from SHARED_KEYS) or this is drift.",
                air[key], ground[key]
            );
        }

        // Every key either agrees above or is a declared per-profile
        // difference. Nothing may be silently absent from both lists.
        const PER_PROFILE_KEYS: &[&str] =
            &["hlsVariant", "webrtcLocalTCPAddress", "webrtcICEServers2"];
        let rendered: Vec<String> = air
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.to_string())
            .collect();
        for key in &rendered {
            assert!(
                SHARED_KEYS.contains(&key.as_str()) || PER_PROFILE_KEYS.contains(&key.as_str()),
                "`{key}` is rendered but classified neither shared nor \
                 per-profile, so the parity gate does not cover it"
            );
        }
        assert_ne!(
            air["hlsVariant"], ground["hlsVariant"],
            "the ground station is pinned to mpegts for measured reasons; if \
             that changed, move hlsVariant into SHARED_KEYS"
        );
    }

    /// The native fMP4 recorder must render on BOTH profiles when enabled: the
    /// drone had no recording of any kind and the ground station had a single
    /// whole-stream `+faststart` MP4 whose moov atom is written only on a clean
    /// exit — so a power cut mid-flight yielded a zero-recoverable file.
    #[test]
    fn recording_renders_the_fmp4_block_and_playback_on_both_profiles() {
        let lan: Vec<String> = vec![];
        let streams = vec![("main".to_string(), "publisher".to_string())];

        for profile in [MediamtxProfile::Air, MediamtxProfile::Ground] {
            let mut params = ConfigParams::new(profile, &lan, &streams);
            params.recording = RecordingParams {
                enabled: true,
                ..RecordingParams::default()
            };
            let v: Value = serde_norway::from_str(&mediamtx_config_yaml(&params)).unwrap();
            let pd = &v["pathDefaults"];

            assert_eq!(pd["record"], true, "{profile:?} must record");
            assert_eq!(pd["recordFormat"], "fmp4", "{profile:?} must be fMP4");
            // The part duration IS the recovery point objective: each part is
            // independently flushed, so an unclean exit loses at most this much.
            assert_eq!(pd["recordPartDuration"], "200ms");
            assert_eq!(pd["recordMaxPartSize"], "50M");
            // One file per minute bounds the blast radius of a corrupt file.
            assert_eq!(pd["recordSegmentDuration"], "60s");
            assert_eq!(pd["recordDeleteAfter"], "24h");
            assert_eq!(
                pd["recordPath"], "/var/ados/recordings/%path/%Y-%m-%d_%H-%M-%S-%f",
                "%f is what keeps two segments opened in the same second apart"
            );
            // Without the playback server the segments are unreachable, so
            // recording without it is a write-only store.
            assert_eq!(v["playback"], true);
            assert_eq!(v["playbackAddress"], ":9996");
            // mediamtx's default REPLACES each frame's timestamp with the
            // current time, destroying capture-time information.
            assert_eq!(pd["useAbsoluteTimestamp"], true);
            // No hook: the reclaim owner is the supervisor janitor, and a
            // second reclaimer would race it to delete the same files.
            assert!(pd.get("runOnRecordSegmentComplete").is_none());
        }
    }

    /// Recording off must still leave the block renderable and playback up —
    /// an operator who just stopped recording still wants the last flight.
    #[test]
    fn recording_disabled_keeps_playback_and_sets_record_false() {
        let lan: Vec<String> = vec![];
        let streams = vec![("main".to_string(), "publisher".to_string())];
        let yaml = mediamtx_config_yaml(&default_params(&lan, &streams));
        let v: Value = serde_norway::from_str(&yaml).unwrap();
        assert_eq!(v["pathDefaults"]["record"], false);
        assert_eq!(v["playback"], true);
    }

    #[test]
    fn config_without_lan_ip_falls_back_and_omits_additional_hosts() {
        let lan: Vec<String> = vec![];
        let streams = vec![("main".to_string(), "publisher".to_string())];
        let yaml = mediamtx_config_yaml(&default_params(&lan, &streams));
        let v: Value = serde_norway::from_str(&yaml).unwrap();
        // No LAN IP → UDP/TCP fall back to :8189 and additional-hosts is absent.
        assert_eq!(v["webrtcLocalUDPAddress"], ":8189");
        assert_eq!(v["webrtcLocalTCPAddress"], "");
        assert!(v.get("webrtcAdditionalHosts").is_none());
    }

    #[test]
    fn non_publisher_source_gets_source_on_demand() {
        let lan: Vec<String> = vec![];
        let streams = vec![
            ("main".to_string(), "publisher".to_string()),
            ("cam2".to_string(), "rtsp://10.0.0.9:554/live".to_string()),
        ];
        let yaml = mediamtx_config_yaml(&default_params(&lan, &streams));
        let v: Value = serde_norway::from_str(&yaml).unwrap();
        assert!(v["paths"]["main"].get("sourceOnDemand").is_none());
        assert_eq!(v["paths"]["cam2"]["sourceOnDemand"], true);
        assert_eq!(v["paths"]["cam2"]["source"], "rtsp://10.0.0.9:554/live");
    }

    #[test]
    fn config_round_trips_through_typed_map() {
        // Sanity: the rendered YAML deserializes into a typed paths map.
        let lan = vec!["10.1.1.5".to_string()];
        let streams = vec![("main".to_string(), "publisher".to_string())];
        let yaml = mediamtx_config_yaml(&default_params(&lan, &streams));
        #[derive(serde::Deserialize)]
        struct Probe {
            paths: BTreeMap<String, Value>,
        }
        let probe: Probe = serde_norway::from_str(&yaml).unwrap();
        assert!(probe.paths.contains_key("main"));
    }

    // --- write_config I/O ----------------------------------------------

    #[tokio::test]
    async fn write_config_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = MediamtxManager::new(dir.path());
        let streams = vec![("main".to_string(), "publisher".to_string())];
        mgr.write_config_with_ips(&streams, &["192.168.1.50".to_string()])
            .await
            .unwrap();
        assert!(mgr.config_path().exists());
        let text = std::fs::read_to_string(mgr.config_path()).unwrap();
        let v: Value = serde_norway::from_str(&text).unwrap();
        assert_eq!(v["rtspAddress"], ":8554");
        assert_eq!(v["paths"]["main"]["source"], "publisher");
    }

    /// The recording knobs must survive the manager → renderer → disk path,
    /// not just the pure renderer: `video.recording` was dead config for its
    /// whole life and a wiring break is exactly how it stays dead.
    #[tokio::test]
    async fn write_config_carries_the_recording_knobs_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = MediamtxManager::for_profile(MediamtxProfile::Ground, dir.path()).with_recording(
            RecordingParams {
                enabled: true,
                root: "/var/ados/recordings".into(),
                delete_after: "6h".into(),
                ..RecordingParams::default()
            },
        );
        mgr.write_config_with_ips(&[("main".to_string(), "publisher".to_string())], &[])
            .await
            .unwrap();
        let text = std::fs::read_to_string(mgr.config_path()).unwrap();
        let v: Value = serde_norway::from_str(&text).unwrap();
        assert_eq!(v["pathDefaults"]["record"], true);
        assert_eq!(v["pathDefaults"]["recordDeleteAfter"], "6h");
        assert_eq!(v["pathDefaults"]["recordFormat"], "fmp4");
        assert_eq!(v["playbackAddress"], ":9996");
    }

    // --- HTTP client against an in-test listener -----------------------

    /// Serve exactly one canned HTTP response on a fresh loopback listener and
    /// return the chosen port.
    async fn serve_once(response: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                // Drain the request (read once is enough for a small GET).
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.flush().await;
                // Drop closes the connection → read_to_EOF on the client side.
            }
        });
        port
    }

    #[tokio::test]
    async fn path_ready_parses_true() {
        let resp = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n\
                    {\"name\":\"main\",\"ready\":true,\"bytesReceived\":4096}";
        let port = serve_once(resp).await;
        let mgr = MediamtxManager::new(std::path::Path::new("/tmp")).with_ports(
            port,
            DEFAULT_RTSP_PORT,
            DEFAULT_WEBRTC_PORT,
            DEFAULT_HLS_PORT,
        );
        assert!(mgr.path_ready("main").await);
    }

    #[tokio::test]
    async fn path_ready_parses_false() {
        let resp = "HTTP/1.1 200 OK\r\n\r\n{\"ready\":false}";
        let port = serve_once(resp).await;
        let mgr = MediamtxManager::new(std::path::Path::new("/tmp")).with_ports(
            port,
            DEFAULT_RTSP_PORT,
            DEFAULT_WEBRTC_PORT,
            DEFAULT_HLS_PORT,
        );
        assert!(!mgr.path_ready("main").await);
    }

    #[tokio::test]
    async fn inbound_bytes_parses_counter() {
        let resp = "HTTP/1.1 200 OK\r\n\r\n{\"ready\":true,\"bytesReceived\":123456}";
        let port = serve_once(resp).await;
        let mgr = MediamtxManager::new(std::path::Path::new("/tmp")).with_ports(
            port,
            DEFAULT_RTSP_PORT,
            DEFAULT_WEBRTC_PORT,
            DEFAULT_HLS_PORT,
        );
        assert_eq!(mgr.inbound_bytes("main").await, Some(123456));
    }

    #[tokio::test]
    async fn inbound_bytes_missing_field_is_none() {
        let resp = "HTTP/1.1 200 OK\r\n\r\n{\"ready\":true}";
        let port = serve_once(resp).await;
        let mgr = MediamtxManager::new(std::path::Path::new("/tmp")).with_ports(
            port,
            DEFAULT_RTSP_PORT,
            DEFAULT_WEBRTC_PORT,
            DEFAULT_HLS_PORT,
        );
        assert_eq!(mgr.inbound_bytes("main").await, None);
    }

    #[tokio::test]
    async fn non_200_is_not_ready_and_no_bytes() {
        let resp = "HTTP/1.1 404 Not Found\r\n\r\n{\"error\":\"not found\"}";
        let port = serve_once(resp).await;
        let mgr = MediamtxManager::new(std::path::Path::new("/tmp")).with_ports(
            port,
            DEFAULT_RTSP_PORT,
            DEFAULT_WEBRTC_PORT,
            DEFAULT_HLS_PORT,
        );
        assert!(!mgr.path_ready("main").await);
        assert_eq!(mgr.inbound_bytes("main").await, None);
    }

    #[tokio::test]
    async fn connection_refused_is_graceful() {
        // Bind a listener to grab a free port, then drop it so the port is
        // (almost certainly) closed — a connect there is refused.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let mgr = MediamtxManager::new(std::path::Path::new("/tmp")).with_ports(
            port,
            DEFAULT_RTSP_PORT,
            DEFAULT_WEBRTC_PORT,
            DEFAULT_HLS_PORT,
        );
        assert!(!mgr.path_ready("main").await);
        assert_eq!(mgr.inbound_bytes("main").await, None);
    }

    #[tokio::test]
    async fn wait_for_tcp_port_succeeds_on_open_listener() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // Keep the listener alive for the probe.
        let ok = wait_for_tcp_port("127.0.0.1", port, Duration::from_secs(2)).await;
        assert!(ok);
        drop(listener);
    }

    #[tokio::test]
    async fn wait_for_tcp_port_times_out_on_closed_port() {
        // Bind an ephemeral port, then drop the listener so the port is closed.
        // Under the parallel test runner a concurrent bind can momentarily reuse
        // that just-freed port number inside the probe window, so retry on a fresh
        // port rather than flaking — five independent ports all being reused at
        // once is not a real outcome.
        for _ in 0..5 {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            if !wait_for_tcp_port("127.0.0.1", port, Duration::from_millis(300)).await {
                return; // closed as expected
            }
        }
        panic!("every freed ephemeral port probed as open — port reuse, not a real reachable port");
    }

    #[test]
    fn find_subslice_basic() {
        assert_eq!(find_subslice(b"abc\r\n\r\nxyz", b"\r\n\r\n"), Some(3));
        assert_eq!(find_subslice(b"no-sep-here", b"\r\n\r\n"), None);
        assert_eq!(find_subslice(b"", b"x"), None);
    }
}
