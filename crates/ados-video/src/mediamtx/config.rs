//! `mediamtx.yml` rendering: the typed config tree and its YAML output.

use serde::Serialize;

use super::*;

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
    /// Kept false on both profiles. With it on, mediamtx skips every RTP
    /// packet of an RTSP publisher until that publisher's first RTCP sender
    /// report, and the ffmpeg encoder's first one lands about 5 s after it
    /// starts publishing, so every encoder spawn, camera recovery and
    /// hero/thumbnail switch blanked `main` for every reader for ~5 s
    /// (measured on the bench: the publisher session carried bytes from
    /// +0.1 s while the path's bytesReceived stayed 0 for 5.0 s). Frame
    /// capture time for latency measurement travels in the wall-clock SEI
    /// the agent injects upstream of mediamtx (`video.wfb.sei_latency`), not
    /// in RTP timestamps.
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
    /// Off. mediamtx enables RTMP (:1935) and SRT (:8890) by default on every
    /// interface; nothing here publishes or reads over either, so they would
    /// only be two more network-facing ways into the stream.
    rtmp: bool,
    srt: bool,
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
    #[serde(rename = "authInternalUsers")]
    auth_internal_users: Vec<AuthInternalUser>,
}

/// One `authInternalUsers` entry: who (`any` = no credential) may do what,
/// from which source addresses (empty = any address).
#[derive(Debug, Serialize)]
struct AuthInternalUser {
    user: String,
    pass: String,
    ips: Vec<String>,
    permissions: Vec<AuthPermission>,
}

#[derive(Debug, Serialize)]
struct AuthPermission {
    action: String,
}

/// Every HTTP listener (WHEP signalling, HLS, the control API, the playback
/// server) binds this address. They are reached only through the agent's
/// credentialed `/whep`, `/hls` and recording routes on the LAN front, so
/// nothing off-box may dial mediamtx directly. The ICE media socket is the one
/// listener that faces the network, because the browser's media flows to it.
const LOOPBACK_BIND: &str = "127.0.0.1";

/// Source addresses mediamtx treats as on-box.
const LOOPBACK_SOURCES: [&str; 2] = ["127.0.0.1", "::1"];

/// The permission table. Upstream's default lets any address publish, which
/// would let a LAN peer push its own stream onto `main` and evict the encoder.
/// Publishing (and the playback, API, metrics and pprof actions) is therefore
/// loopback-only. Reading stays open to any address because the RTSP listener
/// still faces the network: an offload compute node pulls the live feed from
/// `rtsp://<node>:8554/main`, and the HTTP listeners are loopback-bound anyway.
fn auth_internal_users() -> Vec<AuthInternalUser> {
    let permissions = |actions: &[&str]| {
        actions
            .iter()
            .map(|a| AuthPermission {
                action: (*a).to_string(),
            })
            .collect()
    };
    vec![
        AuthInternalUser {
            user: "any".into(),
            pass: String::new(),
            ips: LOOPBACK_SOURCES.iter().map(|s| (*s).to_string()).collect(),
            permissions: permissions(&["publish", "read", "playback", "api", "metrics", "pprof"]),
        },
        AuthInternalUser {
            user: "any".into(),
            pass: String::new(),
            ips: Vec::new(),
            permissions: permissions(&["read"]),
        },
    ]
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
        api_address: format!("{LOOPBACK_BIND}:{}", params.api_port),
        read_timeout: RTSP_IO_TIMEOUT.into(),
        write_timeout: RTSP_IO_TIMEOUT.into(),
        write_queue_size: WRITE_QUEUE_SIZE,
        udp_max_payload_size: UDP_MAX_PAYLOAD_SIZE,
        // The playback server is always listening, even when `record` is off:
        // it is the read surface for whatever segments are already on disk, and
        // an operator who just turned recording off still wants to export the
        // flight they recorded a minute ago.
        playback: true,
        playback_address: format!("{LOOPBACK_BIND}:{}", params.playback_port),
        // All interfaces: the encoder publishes over loopback, but an offload
        // compute node on the LAN pulls the live feed from this listener. Who may
        // publish is restricted by `authInternalUsers` instead.
        rtsp: true,
        rtsp_address: format!(":{}", params.rtsp_port),
        rtsp_transports: vec!["tcp".into()],
        rtmp: false,
        srt: false,
        webrtc: true,
        webrtc_address: format!("{LOOPBACK_BIND}:{}", params.webrtc_port),
        webrtc_allow_origin: "*".into(),
        webrtc_ips_from_interfaces: true,
        webrtc_ips_from_interfaces_list: phys_ifaces,
        webrtc_handshake_timeout: WEBRTC_HANDSHAKE_TIMEOUT.into(),
        webrtc_stun_gather_timeout: STUN_GATHER_TIMEOUT.into(),
        webrtc_local_udp_address: format!(":{WEBRTC_LOCAL_ICE_PORT}"),
        webrtc_local_tcp_address: params.profile.webrtc_local_tcp_address(),
        webrtc_ice_servers2: params.profile.ice_servers(),
        hls: true,
        hls_address: format!("{LOOPBACK_BIND}:{}", params.hls_port),
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
            use_absolute_timestamp: false,
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
        auth_internal_users: auth_internal_users(),
    };

    serde_norway::to_string(&config).expect("mediamtx config serializes")
}
