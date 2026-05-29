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

/// STUN servers for WebRTC ICE NAT traversal. All five are free + unlimited;
/// more candidates means a higher chance of a working ICE pair on cellular
/// carriers and restricted NATs. Harmless on a local LAN.
const STUN_SERVERS: &[&str] = &[
    "stun:stun.l.google.com:19302",
    "stun:stun1.l.google.com:19302",
    "stun:stun2.l.google.com:19302",
    "stun:stun.cloudflare.com:3478",
    "stun:global.stun.twilio.com:3478",
];

/// One `{"url": "stun:..."}` entry of the `webrtcICEServers2` list.
#[derive(Debug, Serialize)]
struct IceServer {
    url: String,
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

/// The full `mediamtx.yml` document. Field names use the mediamtx camelCase
/// keys verbatim via `rename`; the field set + values mirror the Python
/// `MediamtxManager.generate_config`. `webrtcAdditionalHosts` is omitted when
/// no LAN IP was detected (the Python code only adds it when `lan_ips`).
#[derive(Debug, Serialize)]
struct MediamtxConfig {
    #[serde(rename = "logLevel")]
    log_level: String,
    api: bool,
    #[serde(rename = "apiAddress")]
    api_address: String,
    rtsp: bool,
    #[serde(rename = "rtspAddress")]
    rtsp_address: String,
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
    #[serde(rename = "hlsAllowOrigin")]
    hls_allow_origin: String,
    #[serde(
        rename = "webrtcAdditionalHosts",
        skip_serializing_if = "Option::is_none"
    )]
    webrtc_additional_hosts: Option<Vec<String>>,
    paths: std::collections::BTreeMap<String, PathConfig>,
}

/// Inputs to the pure config renderer.
pub struct ConfigParams<'a> {
    pub api_port: u16,
    pub rtsp_port: u16,
    pub webrtc_port: u16,
    pub hls_port: u16,
    /// Detected LAN IPv4 addresses. The first, if any, pins the WebRTC ICE
    /// host candidate; all are advertised as `webrtcAdditionalHosts`. Empty →
    /// the UDP/TCP addresses fall back to `:8189` and the additional-hosts key
    /// is omitted (matches the Python `if lan_ips` guard).
    pub lan_ips: &'a [String],
    /// Stream name → source. `"main" -> "publisher"` for the air-side encoder.
    /// A non-`"publisher"` source gets `sourceOnDemand: true`.
    pub streams: &'a [(String, String)],
}

/// Render the `mediamtx.yml` document for the given parameters.
///
/// Byte-identical to PyYAML is not required (mediamtx parses the YAML); field +
/// value parity is. The WebRTC ICE host UDP/TCP addresses bind to the first LAN
/// IP at port 8189 so Pion advertises exactly that reachable candidate (without
/// the bind, auto-discovery emitted only 127.0.0.1 and browsers could not
/// reach it); when no LAN IP is known they fall back to `:8189`.
pub fn mediamtx_config_yaml(params: &ConfigParams) -> String {
    let local_addr = match params.lan_ips.first() {
        Some(ip) => format!("{ip}:{WEBRTC_LOCAL_ICE_PORT}"),
        None => format!(":{WEBRTC_LOCAL_ICE_PORT}"),
    };

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

    let config = MediamtxConfig {
        log_level: "warn".into(),
        api: true,
        api_address: format!(":{}", params.api_port),
        rtsp: true,
        rtsp_address: format!(":{}", params.rtsp_port),
        webrtc: true,
        webrtc_address: format!(":{}", params.webrtc_port),
        webrtc_allow_origin: "*".into(),
        webrtc_ips_from_interfaces: false,
        webrtc_ips_from_interfaces_list: Vec::new(),
        webrtc_handshake_timeout: "15s".into(),
        webrtc_local_udp_address: local_addr.clone(),
        webrtc_local_tcp_address: local_addr,
        webrtc_ice_servers2: STUN_SERVERS
            .iter()
            .map(|u| IceServer {
                url: (*u).to_string(),
            })
            .collect(),
        hls: true,
        hls_address: format!(":{}", params.hls_port),
        hls_always_remux: true,
        hls_variant: "lowLatency".into(),
        hls_segment_count: 7,
        hls_segment_duration: "1s".into(),
        hls_allow_origin: "*".into(),
        webrtc_additional_hosts: if params.lan_ips.is_empty() {
            None
        } else {
            Some(params.lan_ips.to_vec())
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

/// Manages a `mediamtx` subprocess for WebRTC/RTSP/HLS streaming.
pub struct MediamtxManager {
    api_port: u16,
    rtsp_port: u16,
    webrtc_port: u16,
    hls_port: u16,
    config_path: PathBuf,
    process: Option<ManagedProcess>,
}

impl MediamtxManager {
    /// Construct with the default ports and a config path under `config_dir`
    /// (e.g. a temp dir). The config file is written by [`Self::write_config`].
    pub fn new(config_dir: &Path) -> Self {
        Self {
            api_port: DEFAULT_API_PORT,
            rtsp_port: DEFAULT_RTSP_PORT,
            webrtc_port: DEFAULT_WEBRTC_PORT,
            hls_port: DEFAULT_HLS_PORT,
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

    pub fn rtsp_port(&self) -> u16 {
        self.rtsp_port
    }
    pub fn webrtc_port(&self) -> u16 {
        self.webrtc_port
    }
    pub fn api_port(&self) -> u16 {
        self.api_port
    }
    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// Render the config for `streams` (detecting the LAN IP at call time) and
    /// write it to [`Self::config_path`], creating the parent dir.
    pub fn write_config(&self, streams: &[(String, String)]) -> std::io::Result<()> {
        let lan_ips = detect_lan_ips();
        self.write_config_with_ips(streams, &lan_ips)
    }

    /// Render + write the config with the LAN IPs supplied (keeps the I/O path
    /// testable without a live network).
    pub fn write_config_with_ips(
        &self,
        streams: &[(String, String)],
        lan_ips: &[String],
    ) -> std::io::Result<()> {
        let yaml = mediamtx_config_yaml(&ConfigParams {
            api_port: self.api_port,
            rtsp_port: self.rtsp_port,
            webrtc_port: self.webrtc_port,
            hls_port: self.hls_port,
            lan_ips,
            streams,
        });
        if let Some(parent) = self.config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.config_path, yaml)
    }

    /// Spawn `mediamtx <config>` through [`ManagedProcess`] and gate on the
    /// RTSP listener accepting. Returns `Ok(true)` once the process is up
    /// (whether or not the RTSP gate passed — the listener may still come up
    /// after the timeout; a slow start is logged, not fatal). `write_config`
    /// must have been called first. Idempotent: a still-alive process stays.
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
        ConfigParams {
            api_port: DEFAULT_API_PORT,
            rtsp_port: DEFAULT_RTSP_PORT,
            webrtc_port: DEFAULT_WEBRTC_PORT,
            hls_port: DEFAULT_HLS_PORT,
            lan_ips,
            streams,
        }
    }

    // --- config field + value parity ----------------------------------

    #[test]
    fn config_has_exact_ports_and_main_path() {
        let lan = vec!["192.168.200.115".to_string()];
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
        assert_eq!(v["webrtc"], true);
        assert_eq!(v["hls"], true);
        assert_eq!(v["logLevel"], "warn");
        assert_eq!(v["webrtcAllowOrigin"], "*");
        assert_eq!(v["webrtcHandshakeTimeout"], "15s");

        // WebRTC ICE binding to the LAN IP at 8189.
        assert_eq!(v["webrtcIPsFromInterfaces"], false);
        assert!(v["webrtcIPsFromInterfacesList"]
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(v["webrtcLocalUDPAddress"], "192.168.200.115:8189");
        assert_eq!(v["webrtcLocalTCPAddress"], "192.168.200.115:8189");
        assert_eq!(
            v["webrtcAdditionalHosts"],
            Value::from(vec!["192.168.200.115"])
        );

        // HLS low-latency, 7 segs x 1s, always remux.
        assert_eq!(v["hlsAlwaysRemux"], true);
        assert_eq!(v["hlsVariant"], "lowLatency");
        assert_eq!(v["hlsSegmentCount"], 7);
        assert_eq!(v["hlsSegmentDuration"], "1s");
        assert_eq!(v["hlsAllowOrigin"], "*");

        // The `main` publisher path: source=publisher, no sourceOnDemand.
        assert_eq!(v["paths"]["main"]["source"], "publisher");
        assert!(v["paths"]["main"].get("sourceOnDemand").is_none());
    }

    #[test]
    fn config_has_all_five_stun_servers() {
        let lan: Vec<String> = vec![];
        let streams = vec![("main".to_string(), "publisher".to_string())];
        let yaml = mediamtx_config_yaml(&default_params(&lan, &streams));
        let v: Value = serde_norway::from_str(&yaml).unwrap();
        let ice = v["webrtcICEServers2"].as_array().unwrap();
        let urls: Vec<&str> = ice.iter().map(|s| s["url"].as_str().unwrap()).collect();
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
    }

    #[test]
    fn config_without_lan_ip_falls_back_and_omits_additional_hosts() {
        let lan: Vec<String> = vec![];
        let streams = vec![("main".to_string(), "publisher".to_string())];
        let yaml = mediamtx_config_yaml(&default_params(&lan, &streams));
        let v: Value = serde_norway::from_str(&yaml).unwrap();
        // No LAN IP → UDP/TCP fall back to :8189 and additional-hosts is absent.
        assert_eq!(v["webrtcLocalUDPAddress"], ":8189");
        assert_eq!(v["webrtcLocalTCPAddress"], ":8189");
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

    #[test]
    fn write_config_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = MediamtxManager::new(dir.path());
        let streams = vec![("main".to_string(), "publisher".to_string())];
        mgr.write_config_with_ips(&streams, &["192.168.1.50".to_string()])
            .unwrap();
        assert!(mgr.config_path().exists());
        let text = std::fs::read_to_string(mgr.config_path()).unwrap();
        let v: Value = serde_norway::from_str(&text).unwrap();
        assert_eq!(v["rtspAddress"], ":8554");
        assert_eq!(v["paths"]["main"]["source"], "publisher");
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
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let ok = wait_for_tcp_port("127.0.0.1", port, Duration::from_millis(300)).await;
        assert!(!ok);
    }

    #[test]
    fn find_subslice_basic() {
        assert_eq!(find_subslice(b"abc\r\n\r\nxyz", b"\r\n\r\n"), Some(3));
        assert_eq!(find_subslice(b"no-sep-here", b"\r\n\r\n"), None);
        assert_eq!(find_subslice(b"", b"x"), None);
    }
}
