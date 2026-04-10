"""Configuration models and loader for ADOS Drone Agent."""

from __future__ import annotations

from pathlib import Path
from typing import Any

import yaml
from pydantic import BaseModel, Field, model_validator

# --- Agent ---

class AgentConfig(BaseModel):
    device_id: str = ""
    name: str = "my-drone"
    tier: str = "auto"


# --- MAVLink ---

class SigningConfig(BaseModel):
    enabled: bool = False
    key: str = ""


class EndpointConfig(BaseModel):
    type: str = "websocket"
    host: str = "0.0.0.0"
    port: int = 8765
    enabled: bool = True


class MavlinkConfig(BaseModel):
    serial_port: str = ""
    baud_rate: int = 57600
    system_id: int = 1
    component_id: int = 191
    signing: SigningConfig = SigningConfig()
    endpoints: list[EndpointConfig] = Field(default_factory=lambda: [
        EndpointConfig(type="websocket", port=8765, enabled=True),
    ])


# --- Video ---

class WfbConfig(BaseModel):
    interface: str = ""
    channel: int = 149
    tx_power: int = 25
    fec_k: int = 8
    fec_n: int = 12


class CameraConfig(BaseModel):
    source: str = "csi"
    codec: str = "h264"
    width: int = 1280
    height: int = 720
    fps: int = 30
    bitrate_kbps: int = 4000


class RecordingConfig(BaseModel):
    enabled: bool = False
    path: str = "/var/ados/recordings"
    max_duration_minutes: int = 30


class VideoConfig(BaseModel):
    mode: str = "wfb"
    wfb: WfbConfig = WfbConfig()
    camera: CameraConfig = CameraConfig()
    recording: RecordingConfig = RecordingConfig()
    cloud_relay_url: str = ""  # e.g. rtsp://video.altnautica.com:8554


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
    password: str = "ados1234"
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
    mode: str = "cloud"
    cloud: CloudServerConfig = CloudServerConfig()
    self_hosted: SelfHostedServerConfig = SelfHostedServerConfig()
    telemetry_rate: int = 2
    heartbeat_interval: int = 5
    mqtt_transport: str = "websockets"  # "tcp" or "websockets"
    mqtt_username: str = "ados"
    mqtt_password: str = ""  # Auto-filled from API key in cloud mode


# --- Security ---

class TlsConfig(BaseModel):
    enabled: bool = True
    cert_path: str = "/etc/ados/certs/device.crt"
    key_path: str = "/etc/ados/certs/device.key"
    ca_path: str = "/etc/ados/certs/ca.crt"


class WireguardConfig(BaseModel):
    enabled: bool = False
    config_path: str = "/etc/wireguard/ados.conf"


class ApiSecurityConfig(BaseModel):
    api_key: str = ""
    cors_enabled: bool = True
    cors_origins: list[str] = Field(
        default_factory=lambda: ["*"]
    )


class SecurityConfig(BaseModel):
    tls: TlsConfig = TlsConfig()
    wireguard: WireguardConfig = WireguardConfig()
    api: ApiSecurityConfig = ApiSecurityConfig()
    hmac_enabled: bool = False
    hmac_secret: str = ""


# --- Suites ---

class SuiteConfig(BaseModel):
    manifest_dir: str = "/etc/ados/suites"
    active: str = ""
    ros2_workspace: str = "/opt/ados/ros2_ws"


# --- Scripting ---

class TextCommandsConfig(BaseModel):
    enabled: bool = True
    udp_port: int = 8889
    websocket_port: int = 8890


class ScriptsConfig(BaseModel):
    enabled: bool = True
    script_dir: str = "/var/ados/scripts"
    max_concurrent: int = 3


class RestApiConfig(BaseModel):
    enabled: bool = True
    host: str = "0.0.0.0"
    port: int = 8080


class ScriptingConfig(BaseModel):
    text_commands: TextCommandsConfig = TextCommandsConfig()
    scripts: ScriptsConfig = ScriptsConfig()
    rest_api: RestApiConfig = RestApiConfig()


# --- OTA ---

class OtaConfig(BaseModel):
    channel: str = "stable"
    check_interval: int = 24
    auto_install: bool = False
    github_repo: str = "altnautica/ADOSDroneAgent"
    pip_path: str = "/opt/ados/venv/bin/pip"
    service_name: str = "ados-agent"


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
    flight_log_dir: str = "/var/ados/logs/flights"


# --- Pairing ---

class PairingConfig(BaseModel):
    state_path: str = "/etc/ados/pairing.json"
    convex_url: str = ""  # Convex HTTP endpoint for cloud pairing
    beacon_interval: int = 30  # seconds
    heartbeat_interval: int = 60  # seconds
    code_ttl: int = 900  # 15 minutes


# --- Discovery ---

class DiscoveryConfig(BaseModel):
    mdns_enabled: bool = True
    service_type: str = "_ados._tcp.local."


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


# --- Top-level ---

class ADOSConfig(BaseModel):
    agent: AgentConfig = AgentConfig()
    mavlink: MavlinkConfig = MavlinkConfig()
    video: VideoConfig = VideoConfig()
    network: NetworkConfig = NetworkConfig()
    server: ServerConfig = ServerConfig()
    security: SecurityConfig = SecurityConfig()
    suites: SuiteConfig = SuiteConfig()
    scripting: ScriptingConfig = ScriptingConfig()
    ota: OtaConfig = OtaConfig()
    logging: LoggingConfig = LoggingConfig()
    pairing: PairingConfig = PairingConfig()
    discovery: DiscoveryConfig = DiscoveryConfig()
    vision: VisionConfig = VisionConfig()
    swarm: SwarmConfig = SwarmConfig()

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
        Path("/etc/ados/config.yaml"),
        Path("config.yaml"),
    ])

    raw: dict[str, Any] = {}
    for candidate in candidates:
        if candidate.is_file():
            with open(candidate) as f:
                loaded = yaml.safe_load(f)
                if isinstance(loaded, dict):
                    raw = loaded
            break

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
