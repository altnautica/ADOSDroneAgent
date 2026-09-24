//! Cloud relay configuration.
//!
//! Reads the sections of `/etc/ados/config.yaml` the relay needs. Mirrors the
//! Python config field names + defaults so a config written by the Python agent
//! is read identically here; serde ignores every other section, so the large
//! operator config is untouched. The long-running relay tasks read additional
//! keys; this carries the foundation (the convex relay URL) the chunk-1
//! skeleton resolves at startup.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::mqtt::transport::{BrokerWire, TransportConfig};
use crate::mqtt::WS_PATH;

/// The MQTT in-flight ceiling every relay lane dials with: the publish path is
/// the limit, not the client's internal queue.
pub const RELAY_INFLIGHT: u16 = 1000;
/// The MQTT keep-alive every relay lane dials with.
pub const RELAY_KEEP_ALIVE: Duration = Duration::from_secs(30);

/// Canonical config location, overridable via the `ADOS_CONFIG` env var the
/// systemd unit sets (same convention as the other crates).
pub const CONFIG_YAML: &str = "/etc/ados/config.yaml";

/// The cloud-relay endpoint under `server.cloud.url`, plus the MQTT broker host/port
/// the relays dial.
#[derive(Debug, Clone, Deserialize)]
pub struct CloudSection {
    #[serde(default)]
    pub url: String,
    #[serde(default = "default_mqtt_broker")]
    pub mqtt_broker: String,
    #[serde(default = "default_mqtt_port")]
    pub mqtt_port: u16,
}

fn default_mqtt_broker() -> String {
    crate::mqtt::DEFAULT_BROKER_HOST.to_string()
}
fn default_mqtt_port() -> u16 {
    crate::mqtt::DEFAULT_BROKER_PORT
}
fn default_server_mode() -> String {
    "local".to_string()
}
fn default_mqtt_transport() -> String {
    "websockets".to_string()
}
fn default_self_hosted_mqtt_port() -> u16 {
    8883
}

impl Default for CloudSection {
    fn default() -> Self {
        CloudSection {
            url: String::new(),
            mqtt_broker: default_mqtt_broker(),
            mqtt_port: default_mqtt_port(),
        }
    }
}

/// The `server.self_hosted:` section: an operator's own Convex deployment and
/// MQTT broker. `url` is the convex-URL fallback when `pairing.convex_url` is
/// empty but the operator chose the self_hosted posture, so a self-hosted pair
/// that only wrote `server.self_hosted.url` still beacons. `mqtt_broker` /
/// `mqtt_port` are the broker the relay dials in that posture. Mirrors the
/// Python `SelfHostedServerConfig`.
#[derive(Debug, Clone, Deserialize)]
pub struct SelfHostedSection {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub mqtt_broker: String,
    #[serde(default = "default_self_hosted_mqtt_port")]
    pub mqtt_port: u16,
}

impl Default for SelfHostedSection {
    fn default() -> Self {
        SelfHostedSection {
            url: String::new(),
            mqtt_broker: String::new(),
            mqtt_port: default_self_hosted_mqtt_port(),
        }
    }
}

/// The `server:` section: the cloud endpoint + the relay mode + transport.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerSection {
    #[serde(default = "default_server_mode")]
    pub mode: String,
    #[serde(default)]
    pub cloud: CloudSection,
    #[serde(default)]
    pub self_hosted: SelfHostedSection,
    /// How the relay reaches the broker: `websockets` (MQTT over a TLS
    /// WebSocket on `/mqtt`, the default) or `tcp` (MQTT over TLS on the port).
    #[serde(default = "default_mqtt_transport")]
    pub mqtt_transport: String,
    /// The operator opt-in for explicit log-window cloud export. Default OFF:
    /// the durable on-device store is the source of truth and nothing is exported
    /// to the cloud account unless the operator turns this on. Even when on, an
    /// export only runs on an explicit operator-triggered request — there is no
    /// continuous firehose. Mirrors the Python `server.cloud_logs_enabled`.
    #[serde(default)]
    pub cloud_logs_enabled: bool,
}

impl Default for ServerSection {
    fn default() -> Self {
        ServerSection {
            mode: default_server_mode(),
            cloud: CloudSection::default(),
            self_hosted: SelfHostedSection::default(),
            mqtt_transport: default_mqtt_transport(),
            cloud_logs_enabled: false,
        }
    }
}

/// The `agent:` section. The device identity the relay reports.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentSection {
    #[serde(default)]
    pub device_id: String,
    #[serde(default = "default_agent_name")]
    pub name: String,
    #[serde(default = "default_agent_profile")]
    pub profile: String,
}

fn default_agent_name() -> String {
    "my-drone".to_string()
}
fn default_agent_profile() -> String {
    "auto".to_string()
}

impl Default for AgentSection {
    fn default() -> Self {
        AgentSection {
            device_id: String::new(),
            name: default_agent_name(),
            profile: default_agent_profile(),
        }
    }
}

/// The `pairing:` section: the convex URL the loops POST to + the beacon gate.
#[derive(Debug, Clone, Deserialize)]
pub struct PairingSection {
    #[serde(default)]
    pub convex_url: String,
    #[serde(default = "default_beacon_interval")]
    pub beacon_interval: u32,
    #[serde(default = "default_beacon_enabled")]
    pub beacon_enabled: bool,
}

fn default_beacon_interval() -> u32 {
    30
}
fn default_beacon_enabled() -> bool {
    true
}

impl Default for PairingSection {
    fn default() -> Self {
        PairingSection {
            convex_url: String::new(),
            beacon_interval: default_beacon_interval(),
            beacon_enabled: default_beacon_enabled(),
        }
    }
}

/// Default cadence for the auxiliary-lane status snapshot, in seconds.
fn default_aux_status_interval_s() -> f64 {
    1.0
}

/// Default cadence for the auxiliary-lane identity frame, in seconds. Identity
/// does not change, so it is sent far less often than status; it exists to let a
/// ground station name its peer, and to re-answer that after a restart.
fn default_aux_identity_interval_s() -> f64 {
    10.0
}

/// Default gate for publishing status on the auxiliary lane.
fn default_aux_status_enabled() -> bool {
    true
}

/// Floor on either auxiliary-lane cadence, in seconds.
///
/// A misconfigured `0` (or a negative) would otherwise spin the producer as fast
/// as the scheduler allows and flood a lane shared with video and MAVLink. The
/// floor is applied when the interval is read, so no config value can turn the
/// status producer into a busy loop.
pub const AUX_MIN_INTERVAL_S: f64 = 0.2;

/// The `video.wfb:` slice the auto-pair supervisor and the auxiliary-lane status
/// producer read. Only the typed fields below are read; every other wfb field is
/// tolerated.
#[derive(Debug, Clone, Deserialize)]
pub struct WfbSection {
    /// Whether auto-pair is armed. Default false — the operator arms it via the
    /// GCS / captive portal / REST, and a successful pair disarms it again.
    /// Mirrors the Python `video.wfb.auto_pair_enabled`.
    #[serde(default)]
    pub auto_pair_enabled: bool,
    /// Whether this node publishes its status + identity on the auxiliary lane
    /// so a ground station can describe it. Default on: a relayed node with no
    /// description is the problem this solves, and the frames are small and
    /// rate-limited. An operator who wants the lane carrying only MAVLink turns
    /// this off without disabling the lane itself.
    #[serde(default = "default_aux_status_enabled")]
    pub aux_status_enabled: bool,
    /// Seconds between status snapshots. Clamped by [`AUX_MIN_INTERVAL_S`].
    #[serde(default = "default_aux_status_interval_s")]
    pub aux_status_interval_s: f64,
    /// Seconds between identity frames. Clamped by [`AUX_MIN_INTERVAL_S`].
    #[serde(default = "default_aux_identity_interval_s")]
    pub aux_identity_interval_s: f64,
}

impl Default for WfbSection {
    /// Hand-written rather than derived: a derived `Default` would leave both
    /// intervals at `0.0`, and because the enclosing sections are
    /// `#[serde(default)]`, a config file with no `video.wfb:` block would then
    /// produce a zero-interval producer. The floor in
    /// [`WfbSection::status_interval`] would still catch it, but a default that
    /// is only safe because something downstream clamps it is a trap waiting for
    /// the next reader.
    fn default() -> Self {
        Self {
            auto_pair_enabled: false,
            aux_status_enabled: default_aux_status_enabled(),
            aux_status_interval_s: default_aux_status_interval_s(),
            aux_identity_interval_s: default_aux_identity_interval_s(),
        }
    }
}

impl WfbSection {
    /// The status cadence, floored so no config value can spin the producer.
    pub fn status_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f64(self.aux_status_interval_s.max(AUX_MIN_INTERVAL_S))
    }

    /// The identity cadence, floored on the same basis.
    pub fn identity_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f64(self.aux_identity_interval_s.max(AUX_MIN_INTERVAL_S))
    }
}

fn default_camera_width() -> u32 {
    1280
}
fn default_camera_height() -> u32 {
    720
}

/// The `video.camera:` slice — the encoder's frame size. The offload reconciler
/// advertises this to the compute node so its `ffmpeg` reads fixed
/// `width*height*3` RGB24 frames off the drone's RTSP feed (a mismatch misframes,
/// so this must be the true source size). Mirrors the Python `video.camera`
/// width/height (default 1280x720).
#[derive(Debug, Clone, Deserialize)]
pub struct CameraSection {
    #[serde(default = "default_camera_width")]
    pub width: u32,
    #[serde(default = "default_camera_height")]
    pub height: u32,
}

impl Default for CameraSection {
    fn default() -> Self {
        CameraSection {
            width: default_camera_width(),
            height: default_camera_height(),
        }
    }
}

/// The `video:` section. The nested `wfb` slice (auto-pair) and the `camera`
/// frame size (offload) are read here.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct VideoSection {
    #[serde(default)]
    pub wfb: WfbSection,
    #[serde(default)]
    pub camera: CameraSection,
}

/// The `atlas:` section. Only the enable gate is read here; the cameras /
/// selection / intrinsics are the capture service's concern (`ados-atlas`). The
/// Atlas forwarder reads this gate so a non-Atlas agent does no Atlas work.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AtlasSection {
    /// Whether Atlas world-model capture + forwarding is enabled. Mirrors the
    /// `atlas.enabled` key the capture service reads.
    #[serde(default)]
    pub enabled: bool,
    /// A static compute-node address (`host:port`) to forward to when mDNS
    /// discovery is not available (a segmented / bridged network where multicast
    /// does not cross to the compute node). When set, the forwarder builds the
    /// direct-LAN bearer straight from it instead of browsing mDNS. Overridable
    /// per-run via the `ADOS_ATLAS_COMPUTE_ADDR` env var.
    #[serde(default)]
    pub compute_node_addr: Option<String>,
}

fn default_perception_mode() -> String {
    "auto".to_string()
}

/// The `perception.offload:` slice (drone side): where the heavy detector runs.
#[derive(Debug, Clone, Deserialize)]
pub struct OffloadSection {
    /// Tri-state `auto` | `on` | `off`. `auto` = offload when NPU-less + a
    /// workstation is reachable (the default). An unknown value reads as `auto`
    /// (neither forced-on nor off), so a typo never silently disables offload.
    #[serde(default = "default_perception_mode")]
    pub enabled: String,
    /// A pinned workstation address (`host:port`); empty = auto-discover over
    /// mDNS. Overridable per-run via `ADOS_PERCEPTION_OFFLOAD_ADDR`.
    #[serde(default)]
    pub compute_node_addr: Option<String>,
}

impl Default for OffloadSection {
    fn default() -> Self {
        OffloadSection {
            enabled: default_perception_mode(),
            compute_node_addr: None,
        }
    }
}

impl OffloadSection {
    /// The operator turned offload off explicitly.
    pub fn is_off(&self) -> bool {
        self.enabled.trim().eq_ignore_ascii_case("off")
    }
    /// The operator forced offload on (offload even with a local accelerator).
    pub fn is_forced_on(&self) -> bool {
        self.enabled.trim().eq_ignore_ascii_case("on")
    }
}

/// The `perception.serving:` slice (workstation side): whether this node serves
/// offload for other drones and which detector by default.
#[derive(Debug, Clone, Deserialize)]
pub struct ServingSection {
    /// Tri-state `auto` | `on` | `off`. `auto` = auto-accept + serve (default).
    #[serde(default = "default_perception_mode")]
    pub enabled: String,
    /// The served detector model id; empty = the daemon's default.
    #[serde(default)]
    pub detector_model: Option<String>,
}

impl Default for ServingSection {
    fn default() -> Self {
        ServingSection {
            enabled: default_perception_mode(),
            detector_model: None,
        }
    }
}

impl ServingSection {
    /// Serving is disabled explicitly.
    pub fn is_off(&self) -> bool {
        self.enabled.trim().eq_ignore_ascii_case("off")
    }
}

/// The `perception:` section — two-tier execution config. `offload` is read on a
/// drone, `serving` on a workstation; both default so a fresh agent needs no
/// setup.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PerceptionSection {
    #[serde(default)]
    pub offload: OffloadSection,
    #[serde(default)]
    pub serving: ServingSection,
}

/// The slice of the agent config the cloud relay reads. Every field defaults so
/// a missing section never fails the load.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CloudConfig {
    #[serde(default)]
    pub agent: AgentSection,
    #[serde(default)]
    pub server: ServerSection,
    #[serde(default)]
    pub pairing: PairingSection,
    #[serde(default)]
    pub video: VideoSection,
    #[serde(default)]
    pub atlas: AtlasSection,
    #[serde(default)]
    pub perception: PerceptionSection,
}

/// The `ADOS_ATLAS_ENABLED` env override (truthy = `1` / `true` / `yes` / `on`,
/// case-insensitive). Lets a bench / a unit flip Atlas on or off without editing
/// the yaml, matching the env-override convention the other crates use.
fn atlas_env_override() -> Option<bool> {
    std::env::var("ADOS_ATLAS_ENABLED").ok().map(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// The static compute-node address (`host:port`) to forward Atlas events to when
/// mDNS discovery cannot reach the node: the `ADOS_ATLAS_COMPUTE_ADDR` env var
/// wins, else the `atlas.compute_node_addr` config field. Whitespace / empty is
/// treated as unset (fall back to mDNS).
pub fn atlas_compute_addr(config: &CloudConfig) -> Option<String> {
    if let Ok(v) = std::env::var("ADOS_ATLAS_COMPUTE_ADDR") {
        let t = v.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    config
        .atlas
        .compute_node_addr
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The pinned workstation address (`host:port`) the perception offload targets when
/// set, so the reconciler skips mDNS and offloads straight to it (a segmented / bridged
/// network, or an operator who pinned a specific box). The
/// `ADOS_PERCEPTION_OFFLOAD_ADDR` env var wins, else the
/// `perception.offload.compute_node_addr` field. Whitespace / empty ⇒ unset
/// (auto-discover).
pub fn perception_offload_addr(config: &CloudConfig) -> Option<String> {
    if let Ok(v) = std::env::var("ADOS_PERCEPTION_OFFLOAD_ADDR") {
        let t = v.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    config
        .perception
        .offload
        .compute_node_addr
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

impl CloudConfig {
    /// The convex URL the loops POST to, or empty when the relay is disabled.
    /// Cloud relay is on only for an explicit cloud posture; absent, "local",
    /// or an unknown/typo mode all stay local-first and silent. An allowlist
    /// (not a denylist) so a typo'd mode fails CLOSED — it never beacons.
    ///
    /// `pairing.convex_url` is the canonical source. When it is empty but the
    /// operator chose the self_hosted posture, fall back to
    /// `server.self_hosted.url`: the setup writer historically wrote only the
    /// latter, so without this fallback a self-hosted pair would never beacon
    /// even though the relay is enabled (the "pairs but never beacons" bug).
    pub fn effective_convex_url(&self) -> String {
        if !self.cloud_relay_enabled() {
            return String::new();
        }
        let primary = self.pairing.convex_url.trim();
        if !primary.is_empty() {
            return primary.to_string();
        }
        // Fallback: a self_hosted posture whose URL only landed in
        // server.self_hosted.url (the pre-fix setup writer's behaviour).
        if self.server.mode == "self_hosted" {
            let fallback = self.server.self_hosted.url.trim();
            if !fallback.is_empty() {
                return fallback.to_string();
            }
        }
        String::new()
    }

    /// Whether the configured server mode is an explicit cloud-relay posture.
    /// Matches the supervisor's cloud-relay gate: only `cloud` / `self_hosted`
    /// turn the relay on; everything else (absent default, `local`, unknown)
    /// stays local-first.
    pub fn cloud_relay_enabled(&self) -> bool {
        matches!(self.server.mode.as_str(), "cloud" | "self_hosted")
    }

    /// Whether the operator has opted in to explicit log-window cloud export.
    /// Default-off; combined with the cloud-paired check and an explicit
    /// operator-triggered request, this is the full gate on any export.
    pub fn cloud_logs_enabled(&self) -> bool {
        self.server.cloud_logs_enabled
    }
}

/// The config file the relay reads: the `ADOS_CONFIG` override the systemd unit
/// sets, else the canonical path.
pub fn config_path() -> std::path::PathBuf {
    std::env::var("ADOS_CONFIG")
        .unwrap_or_else(|_| CONFIG_YAML.to_string())
        .into()
}

/// Whether Atlas is enabled RIGHT NOW: the `ADOS_ATLAS_ENABLED` env override
/// when set, else the `atlas.enabled` key read fresh from `path`. A missing,
/// unreadable or unparseable file reads disabled.
///
/// Fresh on every call, not the startup snapshot: the GCS enables Atlas by
/// writing the key and restarting only the capture service, so a forwarder that
/// judged the gate once at relay start would never forward an enabled drone's
/// keyframes until the next reboot.
pub fn atlas_enabled_in(path: &Path) -> bool {
    if let Some(forced) = atlas_env_override() {
        return forced;
    }
    #[derive(Default, Deserialize)]
    struct Raw {
        #[serde(default)]
        atlas: AtlasSection,
    }
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_norway::from_str::<Raw>(&text).ok())
        .map(|raw| raw.atlas.enabled)
        .unwrap_or(false)
}

impl CloudConfig {
    /// The agent profile in the WIRE form the receiver's fleet view
    /// discriminates on (`drone` | `ground-station` | `workstation` |
    /// `compute`).
    ///
    /// The config field is the INTERNAL form and may be `ground_station`
    /// (underscore) or `auto`. `auto`, empty, and anything unrecognized resolve
    /// to `drone`: the resolved profile lives in `/etc/ados/profile.conf` on a
    /// real rig, and a node that means to be a ground station sets its profile.
    /// This is the ONE profile discrimination in the relay, so the bind role,
    /// the advertised profile, the aux identity and the offload gate can never
    /// disagree about what this node is.
    pub fn wire_profile(&self) -> &'static str {
        match self.agent.profile.as_str() {
            "ground_station" | "ground-station" => "ground-station",
            "workstation" => "workstation",
            "compute" => "compute",
            _ => "drone",
        }
    }

    /// The MQTT broker the relay lanes dial for this posture, or `None` when
    /// there is no broker to dial.
    ///
    /// `cloud` dials the managed broker (`server.cloud.*`); `self_hosted` dials
    /// the operator's broker (`server.self_hosted.*`) and ONLY that one — a
    /// self-hosted node with no broker configured has no relay, it never falls
    /// back to the managed broker with its pairing key. Anything else is
    /// local-first. `server.mqtt_transport = "tcp"` is MQTT over TLS on the
    /// port; everything else is a TLS WebSocket, where the MQTT-TLS default port
    /// 8883 is served through the tunnel on 443.
    pub fn mqtt_endpoint(&self) -> Option<(String, u16, BrokerWire)> {
        let (host, port) = match self.server.mode.as_str() {
            "cloud" => (
                self.server.cloud.mqtt_broker.trim(),
                self.server.cloud.mqtt_port,
            ),
            "self_hosted" => (
                self.server.self_hosted.mqtt_broker.trim(),
                self.server.self_hosted.mqtt_port,
            ),
            _ => return None,
        };
        if host.is_empty() {
            return None;
        }
        let wire = if self.server.mqtt_transport.trim() == "tcp" {
            BrokerWire::Tls
        } else {
            BrokerWire::Wss
        };
        let port = if wire == BrokerWire::Wss && port == 8883 {
            443
        } else {
            port
        };
        Some((host.to_string(), port, wire))
    }

    /// The dial config for one relay lane, or `None` when this posture has no
    /// broker. Every lane authenticates as `ados-{device_id}` with the pairing
    /// key; `lane` suffixes the ClientID (`ados-{id}-{lane}`) because a broker
    /// evicts an existing session when a second client presents the same id,
    /// and the MAVLink relay holds the bare `ados-{id}`.
    pub fn relay_transport(&self, lane: Option<&str>, api_key: &str) -> Option<TransportConfig> {
        let (host, port, wire) = self.mqtt_endpoint()?;
        let device_id = &self.agent.device_id;
        let client_id = match lane {
            Some(lane) => format!("ados-{device_id}-{lane}"),
            None => format!("ados-{device_id}"),
        };
        Some(TransportConfig {
            client_id,
            host,
            port,
            wire,
            ws_path: WS_PATH.to_string(),
            username: format!("ados-{device_id}"),
            password: api_key.to_string(),
            inflight: RELAY_INFLIGHT,
            keep_alive: RELAY_KEEP_ALIVE,
        })
    }
}

impl CloudConfig {
    /// Load from the canonical path (or the `ADOS_CONFIG` override). A missing or
    /// unparseable file yields the all-defaults config rather than failing — the
    /// relay must still start to report its own degraded state. This is the real
    /// startup entry, so it also publishes the config-status sidecar: a malformed
    /// config surfaces on the remote Health view, not just in the log.
    pub fn load() -> Self {
        let (config, error) = Self::load_reporting(&config_path());
        ados_config::write_config_status("cloud", error.as_deref());
        config
    }

    /// Load from an explicit path (testable). All-defaults on absence / parse
    /// error. Does NOT publish the config-status sidecar — only the real
    /// [`load`](Self::load) startup path does, so tests never write to the run dir.
    pub fn load_from(path: &Path) -> Self {
        Self::load_reporting(path).0
    }

    /// Load from an explicit path, also returning the parse-error message so the
    /// startup path can publish it. `None` on success or a missing/unreadable
    /// file (a fresh node is not a fault); `Some(msg)` on a present-but-malformed
    /// file.
    fn load_reporting(path: &Path) -> (Self, Option<String>) {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => return (CloudConfig::default(), None),
        };
        ados_config::yaml_reporting(&text, "cloud")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_yaml(name: &str, body: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "ados-cloud-cfg-{}-{}.yaml",
            std::process::id(),
            name
        ));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn missing_file_yields_defaults() {
        let cfg = CloudConfig::load_from(Path::new("/nonexistent/ados/config.yaml"));
        assert_eq!(cfg.server.cloud.url, "");
    }

    #[test]
    fn reads_cloud_section_ignoring_the_rest() {
        let yaml = "\
mavlink:
  port: /dev/ttyACM0
server:
  mode: cloud
  cloud:
    url: https://relay.example/convex
video:
  mode: disabled
";
        let path = temp_yaml("full", yaml);
        let cfg = CloudConfig::load_from(&path);
        assert_eq!(cfg.server.cloud.url, "https://relay.example/convex");
        // Default arm flag is off when no wfb section is present.
        assert!(!cfg.video.wfb.auto_pair_enabled);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn cloud_logs_enabled_defaults_off_and_reads_the_opt_in() {
        // Absent → off.
        let cfg = CloudConfig::default();
        assert!(!cfg.cloud_logs_enabled());

        // Present and true → on.
        let yaml = "\
server:
  mode: cloud
  cloud_logs_enabled: true
";
        let path = temp_yaml("cloudlogs", yaml);
        let cfg = CloudConfig::load_from(&path);
        assert!(cfg.cloud_logs_enabled());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_atlas_gate_is_read_fresh_and_the_env_override_wins() {
        // The env var is process-global; keep every assertion in one test so the
        // set/remove is serial and no parallel test sees a stale override.
        let prev = std::env::var("ADOS_ATLAS_ENABLED").ok();
        std::env::remove_var("ADOS_ATLAS_ENABLED");

        // Absent file / atlas section → off.
        assert!(!atlas_enabled_in(Path::new(
            "/nonexistent/ados/config.yaml"
        )));
        let path = temp_yaml("atlas-gate", "agent:\n  device_id: d1\n");
        assert!(!atlas_enabled_in(&path));

        // The operator enables Atlas while the relay runs: the next read sees it.
        std::fs::write(&path, "atlas:\n  enabled: true\n").unwrap();
        assert!(atlas_enabled_in(&path));
        // ...and disabling it is seen the same way.
        std::fs::write(&path, "atlas:\n  enabled: false\n").unwrap();
        assert!(!atlas_enabled_in(&path));

        // The env override wins over the yaml in both directions.
        std::env::set_var("ADOS_ATLAS_ENABLED", "1");
        assert!(atlas_enabled_in(&path), "env=1 forces on over yaml=false");
        std::fs::write(&path, "atlas:\n  enabled: true\n").unwrap();
        std::env::set_var("ADOS_ATLAS_ENABLED", "false");
        assert!(
            !atlas_enabled_in(&path),
            "env=false forces off over yaml=true"
        );
        let _ = std::fs::remove_file(&path);

        // Restore the prior environment for the rest of the suite.
        match prev {
            Some(v) => std::env::set_var("ADOS_ATLAS_ENABLED", v),
            None => std::env::remove_var("ADOS_ATLAS_ENABLED"),
        }
    }

    #[test]
    fn atlas_compute_addr_prefers_env_then_config() {
        // The env var is process-global; guard + restore it in one serial test.
        let prev = std::env::var("ADOS_ATLAS_COMPUTE_ADDR").ok();
        std::env::remove_var("ADOS_ATLAS_COMPUTE_ADDR");

        let mut cfg = CloudConfig::default();
        assert_eq!(atlas_compute_addr(&cfg), None);

        // A whitespace-only config value is treated as unset.
        cfg.atlas.compute_node_addr = Some("   ".to_string());
        assert_eq!(atlas_compute_addr(&cfg), None);

        // The config field is used when set.
        cfg.atlas.compute_node_addr = Some("10.0.0.9:8092".to_string());
        assert_eq!(atlas_compute_addr(&cfg), Some("10.0.0.9:8092".to_string()));

        // The env var wins over the config field.
        std::env::set_var("ADOS_ATLAS_COMPUTE_ADDR", "192.168.1.5:8092");
        assert_eq!(
            atlas_compute_addr(&cfg),
            Some("192.168.1.5:8092".to_string())
        );

        match prev {
            Some(v) => std::env::set_var("ADOS_ATLAS_COMPUTE_ADDR", v),
            None => std::env::remove_var("ADOS_ATLAS_COMPUTE_ADDR"),
        }
    }

    #[test]
    fn reads_the_wfb_auto_pair_arm_flag() {
        let yaml = "\
video:
  wfb:
    channel: 149
    auto_pair_enabled: true
";
        let path = temp_yaml("wfb", yaml);
        let cfg = CloudConfig::load_from(&path);
        assert!(cfg.video.wfb.auto_pair_enabled);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn effective_convex_url_is_an_allowlist_failing_closed() {
        // The convex URL is loaded from `pairing.convex_url`; the mode decides
        // whether it is surfaced. Build a config body that carries a URL and a
        // chosen mode, then assert the gate per mode.
        let with_mode = |mode: &str| {
            let yaml = format!(
                "\
server:
  mode: {mode}
pairing:
  convex_url: https://relay.example/convex
"
            );
            let path = temp_yaml(&format!("mode-{mode}"), &yaml);
            let cfg = CloudConfig::load_from(&path);
            let url = cfg.effective_convex_url();
            let enabled = cfg.cloud_relay_enabled();
            let _ = std::fs::remove_file(&path);
            (url, enabled)
        };

        // Explicit cloud posture → the URL is surfaced and the relay is on.
        let (url, enabled) = with_mode("cloud");
        assert_eq!(url, "https://relay.example/convex");
        assert!(enabled);

        let (url, enabled) = with_mode("self_hosted");
        assert_eq!(url, "https://relay.example/convex");
        assert!(enabled);

        // Local mode → empty, relay off.
        let (url, enabled) = with_mode("local");
        assert!(url.is_empty());
        assert!(!enabled);

        // An unknown / typo'd mode fails CLOSED — empty, relay off (the
        // allowlist's whole point: a typo never beacons to the cloud).
        let (url, enabled) = with_mode("weird");
        assert!(url.is_empty());
        assert!(!enabled);

        // Absent mode (the all-defaults config defaults the mode to "local")
        // → empty, relay off.
        let cfg = CloudConfig::default();
        assert!(cfg.effective_convex_url().is_empty());
        assert!(!cfg.cloud_relay_enabled());

        // A config with NO server section at all also defaults to off.
        let path = temp_yaml(
            "no-server",
            "pairing:\n  convex_url: https://relay.example/convex\n",
        );
        let cfg = CloudConfig::load_from(&path);
        assert!(cfg.effective_convex_url().is_empty());
        assert!(!cfg.cloud_relay_enabled());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn self_hosted_url_is_the_fallback_when_pairing_convex_url_is_empty() {
        // The "pairs but never beacons" case: a self_hosted posture whose URL
        // only landed in server.self_hosted.url (no pairing.convex_url). The
        // relay must still resolve a URL and beacon.
        let yaml = "\
server:
  mode: self_hosted
  self_hosted:
    url: https://convex-site.operator.example
";
        let path = temp_yaml("selfhosted-fallback", yaml);
        let cfg = CloudConfig::load_from(&path);
        assert!(cfg.cloud_relay_enabled());
        assert_eq!(
            cfg.effective_convex_url(),
            "https://convex-site.operator.example"
        );
        let _ = std::fs::remove_file(&path);

        // pairing.convex_url still wins when both are present.
        let yaml2 = "\
server:
  mode: self_hosted
  self_hosted:
    url: https://fallback.example
pairing:
  convex_url: https://primary.example
";
        let path2 = temp_yaml("selfhosted-primary-wins", yaml2);
        let cfg2 = CloudConfig::load_from(&path2);
        assert_eq!(cfg2.effective_convex_url(), "https://primary.example");
        let _ = std::fs::remove_file(&path2);

        // A cloud posture does NOT borrow the self_hosted URL (the fallback is
        // self_hosted-only): empty pairing.convex_url under cloud → empty.
        let yaml3 = "\
server:
  mode: cloud
  self_hosted:
    url: https://should-not-be-used.example
";
        let path3 = temp_yaml("cloud-no-selfhosted-borrow", yaml3);
        let cfg3 = CloudConfig::load_from(&path3);
        assert!(cfg3.effective_convex_url().is_empty());
        let _ = std::fs::remove_file(&path3);
    }
}
