//! MAVLink service configuration.
//!
//! Reads the `mavlink:` section of `/etc/ados/config.yaml`. Mirrors the Python
//! `MavlinkConfig` (core/config/mavlink.py): the same field names and the same
//! defaults so a config written by the Python agent is read identically here.
//! serde ignores every other section, so the large operator config is
//! untouched.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Canonical config location (overridable via the `ADOS_CONFIG` env var, which
/// the systemd unit sets).
pub const CONFIG_YAML: &str = "/etc/ados/config.yaml";

/// On-disk profile marker the installer writes, and the fallback when the
/// config's `agent.profile` is absent or left on `auto` (overridable via the
/// `ADOS_PROFILE_CONF` env var).
pub const PROFILE_CONF: &str = "/etc/ados/profile.conf";

/// The serial baud of an RC module running its native MAVLink mode. Fixed by
/// the module firmware (distinct from the 420 kbaud CRSF RC framing rate),
/// and deliberately absent from the discovery sweep: the MAVLink-over-ELRS
/// serial source opens only a pinned device, never a guessed one.
pub const CRSF_MAVLINK_BAUD: u32 = 460_800;

/// The conventional MAVLink UDP port the RC module's WiFi backpack streams
/// its MAVLink datagrams to on the host.
pub const BACKPACK_UDP_PORT: u16 = 14550;

fn default_baud_rate() -> u32 {
    57600
}
fn default_source() -> String {
    "auto".to_string()
}
fn default_system_id() -> u8 {
    1
}
fn default_component_id() -> u8 {
    191
}
fn default_endpoint_type() -> String {
    "websocket".to_string()
}
fn default_endpoint_host() -> String {
    "0.0.0.0".to_string()
}
fn default_endpoint_port() -> u16 {
    8765
}
fn default_endpoint_enabled() -> bool {
    true
}
fn default_endpoints() -> Vec<EndpointConfig> {
    vec![EndpointConfig::default()]
}
/// Whether an unauthorized WebSocket handshake is REFUSED rather than merely
/// recorded.
///
/// On. It shipped off while the credential was being rolled out, so that an
/// agent whose clients did not yet present one kept working -- but that meant a
/// paired node logged an unauthorized connection and then served it anyway, and
/// the published documentation described an endpoint that authenticates. The
/// gap between the two was the exposure.
///
/// The rule it enforces, on either pairing state:
///
/// - **unpaired:** on-box callers and the first-boot lifelines (the AP and USB
///   gateway addresses) only; every other caller is refused, because a node
///   with no key has nothing to check a remote caller against;
/// - **paired:** on-box callers, or any caller presenting the pairing key
///   (`X-ADOS-Key`) or a valid WebSocket ticket.
fn default_ws_proxy_enforce_auth() -> bool {
    true
}
/// Whether an unauthorized RAW byte-stream connection (TCP 5760, UDP 14550) is
/// REFUSED rather than merely recorded.
///
/// On. The raw edges carry no credential channel -- there is no handshake and
/// no header on a MAVLink byte stream, so
/// [`crate::proxies::WsProxyAuth::classify`] presents no key at all -- which
/// means every off-box, non-lifeline peer is unauthorized here. Serving them
/// anyway handed full flight-controller write to any host on the same LAN or
/// hotspot of a never-paired node. With this on, those peers are refused
/// unless the operator opened the raw edges with
/// [`default_raw_proxy_lan_access`]; the on-box operator and (while unpaired)
/// the first-boot lifelines are served either way. Turning it off restores the
/// observe-only posture on an unpaired node.
fn default_raw_proxy_enforce_auth() -> bool {
    true
}
/// Whether the raw byte-stream proxies (TCP 5760, UDP 14550) serve off-box
/// peers that cannot authenticate.
///
/// Off. Those edges carry no credential channel, so serving them to the LAN
/// hands flight control to any host on it. Closed, the raw proxies serve the
/// on-box operator and, while the node is unpaired, its first-boot lifelines.
/// An operator who attaches a desktop ground station (QGroundControl, Mission
/// Planner) over the LAN opts in here, deliberately and on record in their
/// config.
fn default_raw_proxy_lan_access() -> bool {
    false
}
/// Whether a client whose origin could not be authenticated is REFUSED the
/// aux-radio uplink rather than merely recorded.
///
/// Off. The uplink fallback is taken only when this node has no local flight
/// controller, i.e. on a ground station relaying a drone that may be flying, so
/// a refusal lands on the operator's screen mid-flight rather than at install
/// time. It is a flag so a fleet that has finished credentialing its clients
/// can close the edge; it is not the default while third-party ground stations
/// are documented as attaching without one.
///
/// Scope note: this gates the `Unauthenticated` origin ONLY. See
/// [`crate::connection::FcConnection::send_client_bytes`] for why `Relayed` is
/// exempt under both settings.
fn default_aux_uplink_enforce_origin() -> bool {
    false
}
fn default_legacy_stream_request() -> bool {
    false
}
fn default_profile() -> String {
    "drone".to_string()
}

/// A network entry point. v1 ships only the `websocket` type.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EndpointConfig {
    #[serde(rename = "type", default = "default_endpoint_type")]
    pub kind: String,
    #[serde(default = "default_endpoint_host")]
    pub host: String,
    #[serde(default = "default_endpoint_port")]
    pub port: u16,
    #[serde(default = "default_endpoint_enabled")]
    pub enabled: bool,
}

impl Default for EndpointConfig {
    fn default() -> Self {
        Self {
            kind: default_endpoint_type(),
            host: default_endpoint_host(),
            port: default_endpoint_port(),
            enabled: default_endpoint_enabled(),
        }
    }
}

/// The `mavlink:` config section.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MavlinkConfig {
    /// The FC transport class the operator picked: `auto` (discover + baud-probe
    /// any candidate serial port), `serial` (the configured `serial_port` +
    /// `baud_rate`), or `udp`/`tcp` (a network transport, with the host:port in
    /// `serial_port` as `udp:host:port` / `tcp:host:port`). Default `auto` so an
    /// un-upgraded config behaves exactly as before. Surfaced on the state
    /// snapshot as `fc_source` so the GCS picker reflects the live choice.
    #[serde(default = "default_source")]
    pub source: String,
    #[serde(default)]
    pub serial_port: String,
    #[serde(default = "default_baud_rate")]
    pub baud_rate: u32,
    #[serde(default = "default_system_id")]
    pub system_id: u8,
    #[serde(default = "default_component_id")]
    pub component_id: u8,
    #[serde(default = "default_endpoints")]
    pub endpoints: Vec<EndpointConfig>,
    /// When true (the default), the direct-GCS WebSocket proxy rejects a
    /// paired-agent connection from an off-box peer that does not present the
    /// stored pairing key or a valid ticket. When false, an unauthorized
    /// connection is logged but still admitted, which is the escape hatch for
    /// an operator whose third-party client can present neither.
    #[serde(default = "default_ws_proxy_enforce_auth")]
    pub ws_proxy_enforce_auth: bool,
    /// When true (the default), the raw byte-stream proxies (TCP 5760, UDP
    /// 14550) refuse a peer that is neither on-box nor (while unpaired) a
    /// first-boot lifeline, unless [`Self::raw_proxy_lan_access`] opens them.
    /// When false, an unpaired node logs such a peer and serves it anyway. See
    /// [`default_raw_proxy_enforce_auth`].
    #[serde(default = "default_raw_proxy_enforce_auth")]
    pub raw_proxy_enforce_auth: bool,
    /// When true, the raw byte-stream proxies serve off-box peers that cannot
    /// authenticate (the operator's LAN ground station); when false (the
    /// default) they serve on-box callers and unpaired-node lifelines only.
    /// See [`default_raw_proxy_lan_access`].
    #[serde(default = "default_raw_proxy_lan_access")]
    pub raw_proxy_lan_access: bool,
    /// When true, a client this router could not authenticate is refused the
    /// aux-radio uplink instead of being logged and relayed anyway. False by
    /// default. The `Relayed` origin is exempt under both settings -- it names
    /// the cloud relay's own declared forwarding, which is the production
    /// remote-command path for a ground-station-relayed drone. See
    /// [`default_aux_uplink_enforce_origin`].
    #[serde(default = "default_aux_uplink_enforce_origin")]
    pub aux_uplink_enforce_origin: bool,
    /// Whether the legacy `REQUEST_DATA_STREAM` group requests are sent
    /// alongside the modern `SET_MESSAGE_INTERVAL` per-message requests on every
    /// stream refresh. OFF by default — see
    /// [`crate::connection::FcConnection::tick_streams`] for the measurement
    /// that made it a flag.
    #[serde(default = "default_legacy_stream_request")]
    pub legacy_stream_request: bool,
    /// Whether the FC-write PIC arbiter gates declared autonomous injectors
    /// (`mavlink.injector_arbitration`). The arbiter (`ados-pic`) runs only on
    /// a ground station, so the gate is built only there; on any other profile
    /// the flag is reported as `inactive_no_local_arbiter` and no injector is
    /// gated, because a gate with no local arbiter would refuse every command
    /// forever. See [`MavlinkConfig::injector_arbitration_state`].
    #[serde(default)]
    pub injector_arbitration: bool,
    /// Whether FC frames addressed to one system go only to the endpoint that
    /// owns it (`mavlink.route_aware`). Off: today's flat fan-out.
    #[serde(default)]
    pub route_aware: bool,
    /// Whether agent-originated frames are signed with an agent-held key
    /// (`mavlink.agent_signing`). See [`crate::signing`].
    #[serde(default)]
    pub agent_signing: bool,
    /// The serial device an ExpressLRS / CRSF RC module is pinned to
    /// (`radio.crsf.device` in the config — the top-level `radio:` section, NOT
    /// the `mavlink:` block, so it is skipped by the section deserializer and
    /// filled in by the loader). Empty = no pin. In `crsf_rc` mode a pinned
    /// device is excluded from FC candidacy entirely: the router must never
    /// open or baud-sweep the RC module's port, or it contends with the CRSF
    /// lane for the same tty. In `mavlink` mode the ownership inverts — the
    /// pin names the serial carrier of the MAVLink-over-ELRS ingest source
    /// this router owns (see [`MavlinkConfig::crsf_mavlink_source`]).
    #[serde(skip)]
    pub crsf_device: String,
    /// The serial devices of the RC lane's modules (`radio.crsf.modules[].port`,
    /// filled in by the loader). Every one is an RC transmitter module the lane
    /// owns, so while the lane is opted in each is excluded from FC candidacy
    /// exactly like [`Self::crsf_device`]. Empty in the single-pin config.
    #[serde(skip)]
    pub crsf_module_ports: Vec<String>,
    /// Whether the `radio.crsf` lane is opted in (`radio.crsf.enabled`, filled
    /// in by the loader like `crsf_device`). Gates the MAVLink-over-ELRS
    /// source resolution: a lane that is not opted in never owns the FC
    /// source slot regardless of its configured mode.
    #[serde(skip)]
    pub crsf_enabled: bool,
    /// What the RC module carries (`radio.crsf.mode`, filled in by the
    /// loader): `crsf_rc` (the RC lane service owns the port; this router only
    /// excludes it), `mavlink` (the module runs its native MAVLink mode and
    /// THIS router owns the carrier as its FC source), or `airport` (a
    /// generic serial data pipe, no router involvement).
    #[serde(skip)]
    pub crsf_mode: String,
    /// The carrier for `mode: mavlink` (`radio.crsf.mavlink_transport`, filled
    /// in by the loader): `serial` (the pinned device at
    /// [`CRSF_MAVLINK_BAUD`]) or `backpack_wifi` (a UDP listen on
    /// [`BACKPACK_UDP_PORT`]).
    #[serde(skip)]
    pub crsf_mavlink_transport: String,
    /// Whether the host->FC command direction is opened for the
    /// MAVLink-over-ELRS source (`radio.crsf.mavlink_command_enabled`, filled
    /// in by the loader). OFF by default and NOT implied by opting the lane in
    /// or selecting `mode: mavlink`: a fresh MAVLink-over-ELRS source is
    /// TELEMETRY-UP / COMMAND-DOWN gated — the router reads inbound MAVLink so
    /// the drone appears and telemetry flows, but installs no writer, so the
    /// host->FC direction is closed until this marker is explicitly set for a
    /// bench-validated command lane. Applies solely to the MAVLink-over-ELRS
    /// source; every other FC source (serial / UDP / TCP / discovery) is
    /// unaffected. See [`crate::connection`] for the writer gate.
    #[serde(skip)]
    pub crsf_mavlink_command_enabled: bool,
    /// The resolved agent profile (`drone` / `ground-station` / other), filled
    /// in by the loader from the top-level `agent:` section with the on-disk
    /// profile marker as the fallback. This router runs on both flight-
    /// controller-bearing profiles, so the drone-only paths gate on it.
    #[serde(skip)]
    pub profile: String,
    /// This node's device id ([`ados_protocol::identity::device_id`]), filled
    /// in by the loader. A relay-proxy request naming another id is not ours.
    #[serde(skip)]
    pub device_id: String,
}

/// The resolved MAVLink-over-ELRS ingest source.
///
/// When the `radio.crsf` block declares the RC module runs its native MAVLink
/// mode, the module is a MAVLink byte carrier — the module firmware owns the
/// CRSF air protocol internally, so nothing host-side parses CRSF on this
/// lane — and this router ingests the carrier as its FC source. The RC lane
/// service (`ados-crsf`) holds off the device entirely in that mode (one
/// owner per port; it stands by at state `ready` with the mode reported and
/// transmits no RC).
///
/// The direction is asymmetric by default: the router reads inbound MAVLink
/// (telemetry up), but the host->FC command-down direction is GATED CLOSED
/// until `radio.crsf.mavlink_command_enabled` is set for a bench-validated
/// command lane. With the marker off (the default) the connection installs no
/// writer, so this source is telemetry-only — see [`crate::connection`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrsfMavlinkSource<'a> {
    /// The module's USB-serial port at the fixed MAVLink-mode baud
    /// ([`CRSF_MAVLINK_BAUD`]). Resolves only from a pinned device: the baud
    /// is not in the discovery sweep, so an unpinned module in MAVLink mode
    /// is an unresolved source — never a discovery guess.
    Serial { device: &'a str },
    /// The module's WiFi-backpack UDP bridge: the backpack streams MAVLink
    /// datagrams to the conventional port on the host, so the router listens
    /// there and locks to the first peer that talks.
    BackpackUdp { port: u16 },
}

impl Default for MavlinkConfig {
    fn default() -> Self {
        Self {
            source: default_source(),
            serial_port: String::new(),
            baud_rate: default_baud_rate(),
            system_id: default_system_id(),
            component_id: default_component_id(),
            endpoints: default_endpoints(),
            ws_proxy_enforce_auth: default_ws_proxy_enforce_auth(),
            raw_proxy_enforce_auth: default_raw_proxy_enforce_auth(),
            raw_proxy_lan_access: default_raw_proxy_lan_access(),
            aux_uplink_enforce_origin: default_aux_uplink_enforce_origin(),
            legacy_stream_request: default_legacy_stream_request(),
            injector_arbitration: false,
            route_aware: false,
            agent_signing: false,
            crsf_device: String::new(),
            crsf_module_ports: Vec::new(),
            crsf_enabled: false,
            crsf_mode: "crsf_rc".to_string(),
            crsf_mavlink_transport: "serial".to_string(),
            crsf_mavlink_command_enabled: false,
            profile: default_profile(),
            device_id: String::new(),
        }
    }
}

mod load;
pub use load::*;

#[cfg(test)]
mod tests;
