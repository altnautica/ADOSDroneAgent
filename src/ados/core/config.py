"""Configuration models and loader for ADOS Drone Agent."""

from __future__ import annotations

from pathlib import Path
from typing import Any, Literal

import yaml
from pydantic import BaseModel, Field, field_validator, model_validator

from ados.core.paths import (
    CA_CERT_PATH,
    CLOUDFLARE_TUNNEL_TOKEN_PATH,
    CONFIG_YAML,
    DEVICE_CERT_PATH,
    DEVICE_KEY_PATH,
    FLIGHT_LOGS_DIR,
    GS_UI_JSON,
    MESH_PSK_PATH,
    PAIRING_JSON,
    RECORDINGS_DIR,
    SCRIPTS_DIR,
    SUITES_DIR,
)

# --- Agent ---

# profile drives air vs ground-station behavior. "auto" triggers the
# boot-time hardware fingerprint in ados.bootstrap.profile_detect.
_ALLOWED_PROFILES = {"auto", "drone", "ground_station"}


class AgentConfig(BaseModel):
    device_id: str = ""
    name: str = "my-drone"
    tier: str = "auto"
    profile: str = "auto"  # auto | drone | ground_station

    @field_validator("profile")
    @classmethod
    def _validate_profile(cls, value: str) -> str:
        if value not in _ALLOWED_PROFILES:
            raise ValueError(
                f"agent.profile must be one of {sorted(_ALLOWED_PROFILES)}, got '{value}'"
            )
        return value


# --- MAVLink ---

class EndpointConfig(BaseModel):
    type: str = "websocket"
    # IPv4 wildcard. The agent's network entry points (REST + MAVLink WS)
    # bind explicit dual-stack sockets at startup via a helper that
    # creates one AF_INET listener AND one AF_INET6 listener, so the
    # `host` here is interpreted as the IPv4 bind address. The IPv6
    # leg is added implicitly by the dual-bind helper. Binding to "::"
    # alone is unreliable across kernels (uvicorn's IPv6-only fallback
    # left IPv4 unreachable on the bench Pi).
    host: str = "0.0.0.0"
    port: int = 8765
    enabled: bool = True


class MavlinkConfig(BaseModel):
    serial_port: str = ""
    baud_rate: int = 57600
    system_id: int = 1
    component_id: int = 191
    endpoints: list[EndpointConfig] = Field(default_factory=lambda: [
        EndpointConfig(type="websocket", port=8765, enabled=True),
    ])

    @model_validator(mode="before")
    @classmethod
    def _drop_legacy_signing(cls, values):
        """Strip legacy mavlink.signing block from old config files.

        The prior SigningConfig scaffolding never held a live key and is
        now removed. MAVLink message signing is owned by the GCS browser;
        the agent does not persist key material.
        """
        if isinstance(values, dict) and "signing" in values:
            values.pop("signing", None)
        return values


# --- Video ---

class WfbConfig(BaseModel):
    interface: str = ""
    channel: int = 149
    # TX power in dBm. RTL8812EU + USB host VBUS topology browns out
    # the dongle above ~18 dBm sustained. Default is the floor for
    # bench bring-up; raise via PUT /api/wfb/tx-power once the link is
    # validated. Hard ceiling is enforced at validation time.
    tx_power_dbm: int = 5
    tx_power_max_dbm: int = 15
    # MCS index passed to wfb_tx -M. Default 1 (low-bitrate, robust).
    # Distinct from tx_power_dbm — earlier code conflated the two.
    mcs_index: int = 1
    # Power-supply topology hint for the WFB radio. Drives the brownout
    # warning in GCS/LCD. host_vbus = USB-A VBUS straight to dongle
    # VDD5.0 (default; what most bench rigs do). powered_hub = external
    # 5 V hub between SBC and dongle. external_5v = dongle has its own
    # 5 V rail wired directly.
    topology: Literal["host_vbus", "powered_hub", "external_5v"] = "host_vbus"
    fec_k: int = 8
    fec_n: int = 12
    # Frequency-band whitelist used by ``select_quietest_channel`` when
    # ``auto_channel_enabled`` is true. U-NII-1 (5180-5240) is almost
    # always quieter than U-NII-3 (5745-5825) in a home/office because
    # consumer routers default to 149-161. Operators in regulatory
    # domains that forbid U-NII-1 should set ``u-nii-3`` here. ``all``
    # asks the scanner to consider every standard channel without a
    # band filter.
    band: Literal["u-nii-1", "u-nii-3", "all"] = "u-nii-1"
    # When true, the agent scans the configured band on every fresh
    # bind and writes the quietest channel into the persisted config
    # before bringing wfb_tx / wfb_rx up. Disable this to pin a
    # specific channel via the ``channel`` field above. The scan is
    # an `iw scan` round-trip (~1-3 s), only run at bind time — never
    # on the steady-state link health tick.
    auto_channel_enabled: bool = True
    # When true, the agent's auto_pair supervisor opens a local bind
    # window on first boot and pairs to whichever unpaired peer responds
    # first on the radio. Flips to false the moment a pair lands so the
    # rig does not silently re-bind to another device after an unpair.
    # Re-enabling requires explicit operator action (REST / CLI / GCS).
    auto_pair_enabled: bool = True
    # Peer device-id and pair timestamp persist on both profiles (drone
    # holds the GS device-id, GS holds the drone device-id). The
    # ground-station-side fields under ground_station.paired_drone_id
    # remain for backward compat with field rigs running older configs;
    # the canonical surface for fresh installs is here.
    paired_with_device_id: str | None = None
    paired_at: str | None = None  # iso timestamp
    # Inject H.264 SEI markers carrying time.time_ns() into the wfb-tee
    # output so the ground side can compute over-the-air video
    # latency. Adds ~30 bytes per VCL NAL (~900 B/s at 30 fps),
    # negligible vs a 4 Mbps stream. On by default so the LCD shows
    # camera→display latency out of the box and the GCS popover can
    # compute true end-to-end via the browser-side SEI parser.
    # To disable, set sei_latency: false in /etc/ados/config.yaml
    # under video.wfb and restart the agent.
    sei_latency: bool = True
    # Operator-facing radio link preset. The WfbManager reads this at
    # startup and overrides mcs_index / fec_k / fec_n with the preset
    # values. Lets a bench operator widen the link without remembering
    # the right K/N/MCS combinations.
    #
    #   conservative (default): MCS=1, FEC=8/12. Low TX power, noisy
    #     bench, 200m range. Safe under host_vbus topology.
    #   balanced: MCS=3, FEC=8/12. Good outdoor link, 500m+, headroom
    #     for RSSI swings. Recommended once topology is powered_hub.
    #   aggressive: MCS=5, FEC=8/10. Excellent SNR, close-in, max
    #     throughput. Will drop the link on a noisy channel.
    #
    # When the preset is left at the default "conservative", the
    # manager respects whatever values are explicitly set on
    # mcs_index / fec_k / fec_n above (so an existing rig with custom
    # values is unaffected by adding the preset field).
    wfb_link_preset: Literal[
        "conservative", "balanced", "aggressive"
    ] = "conservative"
    # Closed-loop adaptive bitrate + FEC ladder, modelled on OpenHD
    # WBLink. When true, a 1 Hz background controller watches the
    # link quality monitor and steps a four-tier ladder
    # (4 Mbps/8-12 -> 3 Mbps/8-14 -> 2 Mbps/8-16 -> 1.2 Mbps/4-12)
    # up or down based on packet loss + RSSI hysteresis. Each tier
    # change costs ~1-2 s of pipeline blackout; the controller
    # paces itself so the link can settle before the next decision.
    # Default off until field-validated; flip via REST or GCS.
    adaptive_bitrate_enabled: bool = False
    # Periodic + reactive coordinated frequency hopping. Operator
    # picks a band (the existing `band` field above) and the agent
    # autonomously moves the WFB-ng link to the quietest channel
    # inside that band on a periodic timer or when the link
    # degrades. Drone-side broadcasts an authenticated
    # HopAnnounce on the reserved control port; GS-side listens
    # and flips synchronously. Self-gating: the drone only flips
    # after it sees a peer ACK, so a half-upgraded pair does not
    # silently lose its link. Default on — the operator does not
    # need to think about channel selection. Disable to pin the
    # link to a fixed channel.
    auto_hop_enabled: bool = True
    # Period in seconds between routine "is there a quieter
    # channel?" rescans. Tuned for the bench: 60 s feels invisible
    # to the operator (one ~300 ms freeze per minute of flight)
    # without sitting on a degraded channel for too long.
    hop_period_seconds: int = 60
    # Reactive hop thresholds. The supervisor triggers an
    # off-schedule migration when the live link quality sample
    # crosses either threshold AND the link has been stable on
    # the current channel for at least 30 s.
    hop_loss_threshold_percent: float = 10.0
    hop_rssi_threshold_dbm: float = -75.0

    @model_validator(mode="before")
    @classmethod
    def _migrate_legacy_tx_power(cls, values):
        """Bridge the old `tx_power` YAML field to `tx_power_dbm`.

        Earlier releases shipped `tx_power: 25` but fed the value to
        `wfb_tx -M`, which is the MCS index, not radio power. Real TX
        power was never set; the dongle ran at driver default (often
        17-20 dBm, the brownout band on host-VBUS topology). The legacy
        value is therefore meaningless and is dropped, not migrated.
        Operators get the new safe default unless they have already
        written `tx_power_dbm` explicitly.
        """
        if not isinstance(values, dict):
            return values
        if "tx_power" in values and "tx_power_dbm" not in values:
            values.pop("tx_power", None)
        elif "tx_power" in values:
            # Both present — drop the legacy alias, keep the new field.
            values.pop("tx_power", None)
        return values

    @model_validator(mode="after")
    def _clamp_tx_power(self):
        if self.tx_power_dbm > self.tx_power_max_dbm:
            self.tx_power_dbm = self.tx_power_max_dbm
        if self.tx_power_dbm < 1:
            self.tx_power_dbm = 1
        return self


class CameraConfig(BaseModel):
    source: str = "csi"
    codec: str = "h264"
    width: int = 1280
    height: int = 720
    fps: int = 30
    bitrate_kbps: int = 4000
    # Operator preference for the wire codec. "auto" picks H.264 by
    # default for browser-compat with WHEP; on boards whose HAL
    # advertises h265_enc the encoder will offer libx265 / mpph265enc
    # / hevc_v4l2m2m, but flipping to H.265 requires the receiver
    # side to also speak it (LCD decoder + browser MediaCapabilities).
    # Switching from h264 to h265 trades ~5-15 ms of wire latency
    # and ~40-50% bitrate against the loss of Firefox / older Chrome
    # Linux support. Default stays h264 until the full dual-codec
    # WHEP negotiation lands.
    codec_preference: Literal["h264", "h265", "auto"] = "auto"


class RecordingConfig(BaseModel):
    enabled: bool = False
    path: str = str(RECORDINGS_DIR)
    max_duration_minutes: int = 30


class VideoConfig(BaseModel):
    mode: str = "wfb"
    wfb: WfbConfig = WfbConfig()
    camera: CameraConfig = CameraConfig()
    recording: RecordingConfig = RecordingConfig()
    cloud_relay_url: str = ""  # e.g. rtsp://video.altnautica.com:8554
    # Decoded-frame cap fed into the LCD GStreamer pipeline (videorate
    # decimation target). Higher = smoother LCD video at the cost of
    # CPU on the SPI render loop. Default 15 matches Pi 4B + Waveshare
    # 3.5" SPI throughput; faster SBCs / hardware decoders / lighter
    # render paths can raise it. Bench-validate before flipping the
    # default in repo.
    lcd_fps_cap: int = Field(
        default=15,
        ge=1,
        le=60,
    )
    # Opt into the in-process GStreamer air-side pipeline that replaces
    # the legacy bash composition of camera-capture + ffmpeg-encoder +
    # mediamtx-air + ffmpeg-tee + python-sei-injector + ffmpeg-RTP with a
    # single PyGObject-driven pipeline writing RTP directly to UDP 5600
    # for wfb_tx. Default off until bench-validated; flip per-rig in
    # /etc/ados/config.yaml under ``video.use_gst_air_pipeline`` and
    # restart the agent. When false, the legacy bash path is unchanged.
    use_gst_air_pipeline: bool = False
    # When the GStreamer pipeline can choose between a hardware encoder
    # (v4l2h264enc on Pi, mpph264enc on Rockchip, omxh264enc / nvv4l2h264enc
    # on Jetson) and the software libx264 fallback, prefer hardware. Set
    # false to force software for A/B benchmarking or when the hardware
    # path is known fragile on a given rig.
    prefer_hw_encoder: bool = True
    # UDP port the GStreamer pipeline emits a second RTP copy to when
    # cloud relay is enabled. mediamtx-air's udpRead ingest binds the
    # same port and republishes as RTSP/WHEP for the browser preview.
    # Only used when ``cloud_relay_url`` is also set; otherwise the
    # branch is muted at the pipeline's identity-element gate and no
    # packets leave the loopback interface.
    cloud_rtp_port: int = 8000


# --- Network ---

class WifiClientConfig(BaseModel):
    enabled: bool = False
    ssid: str = ""
    password: str = ""


class CellularConfig(BaseModel):
    enabled: bool = False
    apn: str = ""


class HotspotConfig(BaseModel):
    enabled: bool = True
    ssid: str = "ADOS-{device_id}"
    # Default WPA2 passphrase used when the agent first brings up its own
    # access point. Predictable so operators can connect from a phone at
    # the bench without reading a generated value off disk. Override in
    # config.yaml for any deployment that needs a unique passphrase.
    password: str = "altnautica"
    channel: int = 6


class NetworkConfig(BaseModel):
    wifi_client: WifiClientConfig = WifiClientConfig()
    cellular: CellularConfig = CellularConfig()
    hotspot: HotspotConfig = HotspotConfig()


# --- Server ---

class CloudServerConfig(BaseModel):
    url: str = "https://convex-site.altnautica.com"
    mqtt_broker: str = "mqtt.altnautica.com"
    mqtt_port: int = 443


class SelfHostedServerConfig(BaseModel):
    url: str = ""
    mqtt_broker: str = ""
    mqtt_port: int = 8883
    api_key: str = ""


class ServerConfig(BaseModel):
    # Cloud posture set by the onboarding wizard's cloud-choice step.
    # `cloud` uses the Altnautica-managed Convex + MQTT backend;
    # `self_hosted` points at the operator's own deployment;
    # `local` disables the cloud relay entirely (Mission Control reaches
    # the agent directly over LAN / hotspot / USB tether).
    mode: Literal["cloud", "self_hosted", "local"] = "cloud"
    cloud: CloudServerConfig = CloudServerConfig()
    self_hosted: SelfHostedServerConfig = SelfHostedServerConfig()
    telemetry_rate: int = 2
    heartbeat_interval: int = 5
    mqtt_transport: str = "websockets"  # "tcp" or "websockets"
    mqtt_username: str = "ados"
    mqtt_password: str = ""  # Auto-filled from API key in cloud mode


# --- Remote access ---

class CloudflareTunnelConfig(BaseModel):
    enabled: bool = False
    token_path: str = str(CLOUDFLARE_TUNNEL_TOKEN_PATH)
    service_name: str = "cloudflared"
    setup_url: str = ""
    api_url: str = ""
    video_whep_url: str = ""
    mavlink_ws_url: str = ""


class RemoteAccessConfig(BaseModel):
    provider: Literal["none", "cloudflare"] = "none"
    public_urls: list[str] = Field(default_factory=list)
    cloudflare: CloudflareTunnelConfig = CloudflareTunnelConfig()


# --- Security ---

class TlsConfig(BaseModel):
    enabled: bool = True
    cert_path: str = str(DEVICE_CERT_PATH)
    key_path: str = str(DEVICE_KEY_PATH)
    ca_path: str = str(CA_CERT_PATH)


class WireguardConfig(BaseModel):
    enabled: bool = False
    config_path: str = "/etc/wireguard/ados.conf"


DEFAULT_CORS_ORIGINS: list[str] = [
    "http://localhost:4000",
    "http://127.0.0.1:4000",
    "http://localhost:4001",
    "http://127.0.0.1:4001",
]


class ApiSecurityConfig(BaseModel):
    api_key: str = ""
    cors_enabled: bool = True
    # Default origins ALWAYS apply unless an explicit override env var
    # is set. Ops files (/etc/ados/security.yaml) only need to populate
    # `cors_origins_extra` to allow additional origins; the defaults
    # are preserved automatically. This avoids the common
    # foot-gun where a deployment override drops localhost:4000 and
    # the local-dev Mission Control can no longer reach the agent.
    cors_origins: list[str] = Field(default_factory=lambda: list(DEFAULT_CORS_ORIGINS))
    # Additional origins added on top of the defaults. Empty by
    # default. The effective allowlist is `defaults | extras`.
    cors_origins_extra: list[str] = Field(default_factory=list)

    @property
    def effective_cors_origins(self) -> list[str]:
        """Return the deduped union of defaults + configured + extras.

        Defaults are ALWAYS merged in so a deployment yaml that sets
        `cors_origins:` to a custom list does not accidentally drop
        the local-dev Mission Control origins. To truly replace the
        allowlist (rare), set the `ADOS_CORS_ORIGINS_OVERRIDE` env
        var to a comma-separated list — that fully replaces.
        """
        import os

        override = os.environ.get("ADOS_CORS_ORIGINS_OVERRIDE", "").strip()
        if override:
            return [o.strip() for o in override.split(",") if o.strip()]
        seen: set[str] = set()
        merged: list[str] = []
        for origin in (
            *DEFAULT_CORS_ORIGINS,
            *self.cors_origins,
            *self.cors_origins_extra,
        ):
            if origin and origin not in seen:
                seen.add(origin)
                merged.append(origin)
        return merged


class SecurityConfig(BaseModel):
    tls: TlsConfig = TlsConfig()
    wireguard: WireguardConfig = WireguardConfig()
    api: ApiSecurityConfig = ApiSecurityConfig()
    hmac_enabled: bool = False
    hmac_secret: str = ""
    # Setup-webapp auth posture. False (default) trusts any browser served
    # the static webapp from this agent's own listening port (same-origin).
    # True requires an X-ADOS-Setup-Token header on every setup mutation;
    # the token is generated at first boot and printed by the CLI.
    setup_token_required: bool = False


# --- Suites ---

class SuiteConfig(BaseModel):
    manifest_dir: str = str(SUITES_DIR)
    active: str = ""
    ros2_workspace: str = "/opt/ados/ros2_ws"


# --- Scripting ---

class TextCommandsConfig(BaseModel):
    enabled: bool = True
    udp_port: int = 8889
    websocket_port: int = 8890


class ScriptsConfig(BaseModel):
    enabled: bool = True
    script_dir: str = str(SCRIPTS_DIR)
    max_concurrent: int = 3


class RestApiConfig(BaseModel):
    enabled: bool = True
    # IPv4 wildcard. See EndpointConfig.host comment — the actual
    # listener binds dual-stack via a separate helper that creates
    # both AF_INET and AF_INET6 sockets at startup.
    host: str = "0.0.0.0"
    port: int = 8080


class ScriptingConfig(BaseModel):
    text_commands: TextCommandsConfig = TextCommandsConfig()
    scripts: ScriptsConfig = ScriptsConfig()
    rest_api: RestApiConfig = RestApiConfig()
    # Optional explicit Mission Control URL surfaced through the setup
    # facade. When empty, the agent only advertises localhost:4000 to
    # operators who reached the setup webapp from localhost; everyone
    # else sees no link. Set this if Mission Control is reachable at a
    # known address (LAN IP, mDNS, tunnel, etc.).
    mission_control_url: str = ""


# --- OTA ---

class OtaConfig(BaseModel):
    channel: str = "stable"
    check_interval: int = 24
    auto_install: bool = False
    github_repo: str = "altnautica/ADOSDroneAgent"
    pip_path: str = "/opt/ados/venv/bin/pip"
    service_name: str = "ados-supervisor"


# --- Vision ---

class VisionConfig(BaseModel):
    enabled: bool = False
    backend: str = "auto"  # auto, rknn, tensorrt, opencv_dnn, tflite
    confidence_threshold: float = 0.5
    models_dir: str = "/opt/ados/models/vision"
    models_cache_max_mb: int = 500
    registry_url: str = "https://raw.githubusercontent.com/altnautica/ADOSMissionControl/main/public/models/registry.json"
    auto_download: bool = True


# --- Logging ---

class LoggingConfig(BaseModel):
    level: str = "info"
    max_size_mb: int = 50
    keep_count: int = 5
    flight_log_dir: str = str(FLIGHT_LOGS_DIR)


# --- Pairing ---

class PairingConfig(BaseModel):
    state_path: str = str(PAIRING_JSON)
    convex_url: str = ""  # Convex HTTP endpoint for cloud pairing
    beacon_interval: int = 30  # seconds
    heartbeat_interval: int = 60  # seconds
    single_process_cloud_enabled: bool = False
    code_ttl: int = 900  # 15 minutes
    # Cloud pair beacon is opt-in. When False (default) the agent
    # only accepts local LAN pair via /api/pairing/claim. Operators
    # who want cross-network discovery flip this to True via the
    # setup webapp or config.yaml. Heartbeats after pair still flow
    # whenever convex_url is set and the agent is paired.
    beacon_enabled: bool = False


# --- Discovery ---

class DiscoveryConfig(BaseModel):
    mdns_enabled: bool = True
    service_type: str = "_ados._tcp.local."


# --- ROS 2 ---

class RosConfig(BaseModel):
    enabled: bool = False
    domain_id: int = 0
    middleware: str = "zenoh"           # zenoh | cyclonedds
    profile: str = "minimal"           # minimal | vio | mapping | custom
    image_name: str = "ados-ros"
    image_tag: str = "jazzy"
    foxglove_port: int = 8766          # 8765 taken by MAVLink WS proxy
    workspace_path: str = "/opt/ados/ros-ws"
    offline_image_path: str = "/opt/ados/ros-offline/jazzy-base.tar.zst"
    memory_limit_mb: int = 4096
    cpu_limit: float = 2.0

    @field_validator("middleware")
    @classmethod
    def _validate_middleware(cls, value: str) -> str:
        allowed = {"zenoh", "cyclonedds"}
        if value not in allowed:
            raise ValueError(f"ros.middleware must be one of {sorted(allowed)}, got '{value}'")
        return value

    @field_validator("profile")
    @classmethod
    def _validate_profile(cls, value: str) -> str:
        allowed = {"minimal", "vio", "mapping", "custom"}
        if value not in allowed:
            raise ValueError(f"ros.profile must be one of {sorted(allowed)}, got '{value}'")
        return value


# --- Swarm ---

class LoraConfig(BaseModel):
    interface: str = ""
    frequency: int = 915000000
    bandwidth: int = 125000
    spreading_factor: int = 7


class WifiDirectConfig(BaseModel):
    enabled: bool = False
    interface: str = ""


class SwarmConfig(BaseModel):
    enabled: bool = False
    lora: LoraConfig = LoraConfig()
    wifi_direct: WifiDirectConfig = WifiDirectConfig()
    role: str = "auto"
    default_formation: str = "line"
    default_spacing: int = 10


# --- Ground Station ---

# ground_station fields live in the Pydantic model so they validate,
# round-trip through save cycles, and show up in config dumps. An earlier
# layout wrote `paired_drone_id` and `paired_at` to `/etc/ados/config.yaml`
# via direct YAML manipulation in pair_manager.py while ADOSConfig had
# `extra="ignore"`, and `share_uplink` lived in a side-file at
# `/etc/ados/ground-station-ui.json`. The migrator in `load_config()` picks
# the legacy side-file value up once and preserves the file on disk.

class GroundStationUiConfig(BaseModel):
    """OLED + buttons + screens UI config for the ground-station profile.

    Pulled out of the legacy `/etc/ados/ground-station-ui.json` side-file
    into the Pydantic model so it round-trips through save cycles and is
    consumed live by oled_service and button_service. The legacy file is
    migrated once at load time and preserved on disk for rollback.

    Field shapes are intentionally loose (`dict`) because the OLED, button
    mapping, and screen order schemas are still evolving. The REST handlers
    and services know the keys they care about.
    """

    oled: dict = Field(default_factory=dict)
    buttons: dict = Field(default_factory=dict)
    screens: dict = Field(default_factory=dict)


class WfbRelayConfig(BaseModel):
    """Relay-role WFB forwarder settings.

    On `relay` nodes, `wfb_rx -f` resolves the receiver via mDNS on the
    batman-adv interface and forwards fragments to its UDP listen port.
    """

    receiver_mdns_service: str = "_ados-receiver._tcp"
    receiver_port: int = 5800


class WfbReceiverConfig(BaseModel):
    """Receiver-role WFB aggregator settings.

    On `receiver` nodes, `wfb_rx -a` listens on `listen_port` for relay
    forwards and FEC-combines them with a local adapter stream if
    `accept_local_nic` is true.
    """

    listen_port: int = 5800
    accept_local_nic: bool = True


class MeshConfig(BaseModel):
    """batman-adv local mesh transport settings.

    `carrier` is the L2 technology on the second USB WiFi dongle:
    802.11s (native mesh, preferred) or IBSS (ad-hoc, fallback for
    drivers without 802.11s). `mesh_id` and `shared_key_path` gate the
    deployment so two adjacent sites on the same channel stay isolated.
    `shared_key_path` defaults to a device-derived key written on first
    boot by mesh_manager (0o600, never logged).
    """

    interface_override: str | None = None
    carrier: Literal["802.11s", "ibss"] = "802.11s"
    mesh_id: str | None = None
    shared_key_path: str = str(MESH_PSK_PATH)
    channel: int = 1  # 2.4 GHz ch 1 default for mesh dongle
    bat_iface: str = "bat0"


class GroundStationConfig(BaseModel):
    share_uplink: bool = False
    paired_drone_id: str | None = None
    paired_at: str | None = None  # iso timestamp
    ui: GroundStationUiConfig = Field(default_factory=GroundStationUiConfig)
    # gate the cloud_relay_bridge live state IPC read so a quick rollback
    # to the stub VehicleState is possible if the wiring causes regressions
    # in the field. Default True.
    use_live_state_ipc: bool = True

    # distributed receive role. `direct` is the single-node path and runs
    # wfb_rx the way a standalone ground station does. `relay` forwards
    # WFB fragments to a receiver over batman-adv. `receiver` aggregates
    # fragments from the local NIC and from remote relays and publishes
    # the combined FEC-repaired stream for the mediamtx pipeline.
    role: Literal["direct", "relay", "receiver"] = "direct"
    # Whether this node should advertise its uplink as a batman-adv
    # gateway. `auto` lets the mesh_manager decide based on actual
    # uplink health.
    cloud_uplink: Literal["auto", "force_on", "force_off"] = "auto"
    wfb_relay: WfbRelayConfig = WfbRelayConfig()
    wfb_receiver: WfbReceiverConfig = WfbReceiverConfig()
    mesh: MeshConfig = MeshConfig()


# --- UI ---

# Mirrors `ados.setup.models.UiConfig` shape so the persisted YAML
# round-trips through both the setup-facade payload model and the
# top-level config model. Defined inline (not imported from setup) so
# `ados.core.config` stays free of inbound dependencies on the setup
# package and the import graph remains a tree, not a cycle.
class UiConfig(BaseModel):
    """UI presentation settings persisted on disk.

    `theme` drives the SPI LCD dashboard palette. Reads happen on every
    render tick via `ados.services.ui.theme.current_palette()`, so a
    flip from `dark` to `light` takes effect immediately without a
    service restart.
    """

    theme: Literal["dark", "light"] = "dark"


# --- Top-level ---

class ADOSConfig(BaseModel):
    agent: AgentConfig = AgentConfig()
    mavlink: MavlinkConfig = MavlinkConfig()
    video: VideoConfig = VideoConfig()
    network: NetworkConfig = NetworkConfig()
    server: ServerConfig = ServerConfig()
    remote_access: RemoteAccessConfig = RemoteAccessConfig()
    security: SecurityConfig = SecurityConfig()
    suites: SuiteConfig = SuiteConfig()
    scripting: ScriptingConfig = ScriptingConfig()
    ota: OtaConfig = OtaConfig()
    logging: LoggingConfig = LoggingConfig()
    pairing: PairingConfig = PairingConfig()
    discovery: DiscoveryConfig = DiscoveryConfig()
    vision: VisionConfig = VisionConfig()
    swarm: SwarmConfig = SwarmConfig()
    ground_station: GroundStationConfig = GroundStationConfig()
    ros: RosConfig = RosConfig()
    ui: UiConfig = Field(default_factory=UiConfig)

    model_config = {"extra": "ignore"}

    @model_validator(mode="before")
    @classmethod
    def fill_device_id(cls, data: Any) -> Any:
        """Auto-generate device_id if empty."""
        if isinstance(data, dict):
            agent = data.get("agent", {})
            if isinstance(agent, dict) and not agent.get("device_id"):
                import uuid
                agent["device_id"] = str(uuid.uuid4())[:8]
                data["agent"] = agent
        return data


# --- Legacy migrators ---

# One-shot per-process guard. Keeps the INFO log from spamming even
# though the migrator is cheap and idempotent after the first run.
_SHARE_UPLINK_MIGRATED: bool = False
_GS_UI_MIGRATED: bool = False

_LEGACY_GS_UI_PATH = GS_UI_JSON
_GS_UI_KEYS = ("oled", "buttons", "screens")


def _migrate_share_uplink_from_legacy_json(
    raw: dict[str, Any],
    yaml_path: Path | None,
) -> dict[str, Any]:
    """Pull `share_uplink` out of the legacy ground-station-ui.json side-file.

    Runs at most once per process (guarded by `_SHARE_UPLINK_MIGRATED`)
    and is a no-op if:
    - the legacy file does not exist, OR
    - the legacy file has no `share_uplink` key, OR
    - `raw['ground_station']['share_uplink']` is already set (Pydantic
      value wins).

    On a live migration the resolved value is written into `raw`
    in-memory AND flushed back to the on-disk YAML so later reads see
    the Pydantic field without needing the legacy file. The legacy
    JSON is preserved on disk for rollback and audit.
    """
    global _SHARE_UPLINK_MIGRATED
    if _SHARE_UPLINK_MIGRATED:
        return raw

    try:
        if not _LEGACY_GS_UI_PATH.is_file():
            _SHARE_UPLINK_MIGRATED = True
            return raw

        import json

        try:
            legacy_data = json.loads(
                _LEGACY_GS_UI_PATH.read_text(encoding="utf-8")
            )
        except (OSError, ValueError):
            _SHARE_UPLINK_MIGRATED = True
            return raw

        if not isinstance(legacy_data, dict):
            _SHARE_UPLINK_MIGRATED = True
            return raw

        if "share_uplink" not in legacy_data:
            _SHARE_UPLINK_MIGRATED = True
            return raw

        gs_section = raw.get("ground_station")
        if not isinstance(gs_section, dict):
            gs_section = {}
        if "share_uplink" in gs_section:
            # Pydantic config already has a value. Do not overwrite.
            _SHARE_UPLINK_MIGRATED = True
            return raw

        legacy_value = bool(legacy_data.get("share_uplink", False))
        gs_section["share_uplink"] = legacy_value
        raw["ground_station"] = gs_section

        # Flush to disk so subsequent loads do not need the legacy file.
        # Best-effort: on failure we still return the in-memory merge.
        if yaml_path is not None:
            try:
                to_write: dict[str, Any] = {}
                if yaml_path.is_file():
                    with open(yaml_path, encoding="utf-8") as fh:
                        loaded = yaml.safe_load(fh)
                    if isinstance(loaded, dict):
                        to_write = loaded
                disk_gs = to_write.get("ground_station")
                if not isinstance(disk_gs, dict):
                    disk_gs = {}
                disk_gs["share_uplink"] = legacy_value
                to_write["ground_station"] = disk_gs

                body = yaml.safe_dump(
                    to_write,
                    sort_keys=False,
                    default_flow_style=False,
                )
                yaml_path.parent.mkdir(parents=True, exist_ok=True)
                tmp_path = yaml_path.with_suffix(yaml_path.suffix + ".tmp")
                tmp_path.write_text(body, encoding="utf-8")
                import os as _os
                _os.replace(str(tmp_path), str(yaml_path))
            except (OSError, yaml.YAMLError):
                # Non-fatal. In-memory value still applies for this run.
                pass

        # Log once. Use plain logging to avoid a circular import on
        # `ados.core.logging`, which itself may call `load_config()`.
        import logging as _logging

        _logging.getLogger("ados.core.config").info(
            f"migrated share_uplink from {GS_UI_JSON} (legacy file preserved)"
        )
    finally:
        _SHARE_UPLINK_MIGRATED = True

    return raw


def _migrate_gs_ui_from_legacy_json(
    raw: dict[str, Any],
    yaml_path: Path | None,
) -> dict[str, Any]:
    """Pull oled/buttons/screens out of the legacy ground-station-ui.json side-file.

    Same shape as `_migrate_share_uplink_from_legacy_json`. Per-key check:
    if `raw['ground_station']['ui'][key]` is already set, do not overwrite.
    Legacy file is preserved on disk for rollback.
    """
    global _GS_UI_MIGRATED
    if _GS_UI_MIGRATED:
        return raw

    try:
        if not _LEGACY_GS_UI_PATH.is_file():
            _GS_UI_MIGRATED = True
            return raw

        import json

        try:
            legacy_data = json.loads(
                _LEGACY_GS_UI_PATH.read_text(encoding="utf-8")
            )
        except (OSError, ValueError):
            _GS_UI_MIGRATED = True
            return raw

        if not isinstance(legacy_data, dict):
            _GS_UI_MIGRATED = True
            return raw

        gs_section = raw.get("ground_station")
        if not isinstance(gs_section, dict):
            gs_section = {}
        ui_section = gs_section.get("ui")
        if not isinstance(ui_section, dict):
            ui_section = {}

        merged_any = False
        for key in _GS_UI_KEYS:
            if key in legacy_data and isinstance(legacy_data[key], dict):
                if key not in ui_section:
                    ui_section[key] = legacy_data[key]
                    merged_any = True

        if not merged_any:
            _GS_UI_MIGRATED = True
            return raw

        gs_section["ui"] = ui_section
        raw["ground_station"] = gs_section

        if yaml_path is not None:
            try:
                to_write: dict[str, Any] = {}
                if yaml_path.is_file():
                    with open(yaml_path, encoding="utf-8") as fh:
                        loaded = yaml.safe_load(fh)
                    if isinstance(loaded, dict):
                        to_write = loaded
                disk_gs = to_write.get("ground_station")
                if not isinstance(disk_gs, dict):
                    disk_gs = {}
                disk_ui = disk_gs.get("ui")
                if not isinstance(disk_ui, dict):
                    disk_ui = {}
                for key in _GS_UI_KEYS:
                    if key in ui_section and key not in disk_ui:
                        disk_ui[key] = ui_section[key]
                disk_gs["ui"] = disk_ui
                to_write["ground_station"] = disk_gs

                body = yaml.safe_dump(
                    to_write,
                    sort_keys=False,
                    default_flow_style=False,
                )
                yaml_path.parent.mkdir(parents=True, exist_ok=True)
                tmp_path = yaml_path.with_suffix(yaml_path.suffix + ".tmp")
                tmp_path.write_text(body, encoding="utf-8")
                import os as _os
                _os.replace(str(tmp_path), str(yaml_path))
            except (OSError, yaml.YAMLError):
                pass

        import logging as _logging

        _logging.getLogger("ados.core.config").info(
            "migrated ground_station.ui (oled/buttons/screens) from "
            f"{GS_UI_JSON} (legacy file preserved)"
        )
    finally:
        _GS_UI_MIGRATED = True

    return raw


def _deep_merge(base: dict[str, Any], override: dict[str, Any]) -> dict[str, Any]:
    """Merge override into base recursively."""
    merged = base.copy()
    for key, val in override.items():
        if key in merged and isinstance(merged[key], dict) and isinstance(val, dict):
            merged[key] = _deep_merge(merged[key], val)
        else:
            merged[key] = val
    return merged


def load_config(path: str | Path | None = None) -> ADOSConfig:
    """Load config from YAML file, merging with defaults.

    Search order:
    1. Explicit path argument
    2. /etc/ados/config.yaml
    3. ./config.yaml
    4. Pure defaults (no file)
    """
    candidates: list[Path] = []
    if path:
        candidates.append(Path(path))
    candidates.extend([
        CONFIG_YAML,
        Path("config.yaml"),
    ])

    raw: dict[str, Any] = {}
    picked_path: Path | None = None
    for candidate in candidates:
        if candidate.is_file():
            with open(candidate) as f:
                loaded = yaml.safe_load(f)
                if isinstance(loaded, dict):
                    raw = loaded
            picked_path = candidate
            break

    # Legacy migration: pull share_uplink out of the pre-Phase-4
    # side-file into the Pydantic-backed ground_station section. Idempotent,
    # runs at most once per process.
    raw = _migrate_share_uplink_from_legacy_json(raw, picked_path)
    raw = _migrate_gs_ui_from_legacy_json(raw, picked_path)

    # Load defaults.yaml from package data
    import importlib.resources
    defaults: dict[str, Any] = {}
    try:
        defaults_ref = importlib.resources.files("ados.core").joinpath("defaults.yaml")
        defaults_text = defaults_ref.read_text(encoding="utf-8")
        loaded = yaml.safe_load(defaults_text)
        if isinstance(loaded, dict):
            defaults = loaded
    except (FileNotFoundError, TypeError):
        pass

    merged = _deep_merge(defaults, raw)
    return ADOSConfig(**merged)
