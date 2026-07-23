"""Root ADOSConfig model — aggregates every domain section."""

from __future__ import annotations

from typing import Any

from pydantic import BaseModel, Field, model_validator

from .agent import AgentConfig
from .api import ApiConfig
from .cloud import RemoteAccessConfig, ServerConfig
from .ground_station import GroundStationConfig
from .mavlink import MavlinkConfig
from .network import NetworkConfig
from .radio import RadioConfig
from .security import SecurityConfig
from .system import (
    AtlasConfig,
    DiscoveryConfig,
    LoggingConfig,
    PairingConfig,
    PerceptionConfig,
    SwarmConfig,
    UiConfig,
    VisionConfig,
)
from .video import VideoConfig

# Dotted config paths whose stored VALUE is a credential (an API key, a
# password, a secret-file path). One list feeds two surfaces so they can never
# drift apart:
#   * the schema emitter marks each path ``x-secret: true`` on its property
#     node, so a schema-driven UI renders set/not-set instead of the value;
#   * the ``GET /api/config`` read redacts each path to the ``***`` sentinel
#     and the ``PUT`` refuses a write of that sentinel back onto a secret.
# Declare a new secret field here (and regenerate the committed schema) and
# every read surface covers it automatically — there is no second list to
# keep in step.
SECRET_PATHS: tuple[str, ...] = (
    "security.tls.key_path",
    "security.api.api_key",
    "security.wireguard.config_path",
    "server.self_hosted.api_key",
    "security.hmac_secret",
    "server.mqtt_password",
    "network.wifi_client.password",
    "network.hotspot.password",
)


class ADOSConfig(BaseModel):
    agent: AgentConfig = AgentConfig()
    mavlink: MavlinkConfig = MavlinkConfig()
    video: VideoConfig = VideoConfig()
    network: NetworkConfig = NetworkConfig()
    radio: RadioConfig = RadioConfig()
    server: ServerConfig = ServerConfig()
    remote_access: RemoteAccessConfig = RemoteAccessConfig()
    security: SecurityConfig = SecurityConfig()
    api: ApiConfig = ApiConfig()
    logging: LoggingConfig = LoggingConfig()
    pairing: PairingConfig = PairingConfig()
    discovery: DiscoveryConfig = DiscoveryConfig()
    vision: VisionConfig = VisionConfig()
    atlas: AtlasConfig = AtlasConfig()
    perception: PerceptionConfig = PerceptionConfig()
    swarm: SwarmConfig = SwarmConfig()
    ground_station: GroundStationConfig = GroundStationConfig()
    ui: UiConfig = Field(default_factory=UiConfig)

    model_config = {"extra": "ignore"}

    @model_validator(mode="before")
    @classmethod
    def fill_device_id(cls, data: Any) -> Any:
        """Fill device_id if empty, preferring the persisted /etc/ados/device-id.

        Reading the persisted identity (instead of minting a throwaway UUID on
        every validation) keeps the short device_id deterministic across
        restarts and prefix-consistent with the WFB peer-id derived from the
        same file. Only mints a fallback when no persisted id is available.
        """
        if isinstance(data, dict):
            agent = data.get("agent", {})
            if isinstance(agent, dict) and not agent.get("device_id"):
                short = ""
                try:
                    from ados.core.identity import get_or_create_device_id
                    short = get_or_create_device_id()[:8]
                except Exception:
                    short = ""
                if not short:
                    import uuid
                    short = uuid.uuid4().hex[:8]
                agent["device_id"] = short
                data["agent"] = agent
        return data
