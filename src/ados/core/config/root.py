"""Root ADOSConfig model — aggregates every domain section."""

from __future__ import annotations

from typing import Any

from pydantic import BaseModel, Field, model_validator

from .agent import AgentConfig
from .api import ApiConfig
from .battery import BatteryConfig
from .cloud import RemoteAccessConfig, ServerConfig
from .ground_station import GroundStationConfig
from .mavlink import MavlinkConfig
from .network import NetworkConfig
from .radio import RadioConfig
from .security import SecurityConfig
from .system import (
    DiscoveryConfig,
    LoggingConfig,
    PairingConfig,
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
    "security.api.api_key",
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
    swarm: SwarmConfig = SwarmConfig()
    battery: BatteryConfig = BatteryConfig()
    ground_station: GroundStationConfig = GroundStationConfig()
    ui: UiConfig = Field(default_factory=UiConfig)

    model_config = {"extra": "ignore"}

    @model_validator(mode="before")
    @classmethod
    def fill_device_id(cls, data: Any) -> Any:
        """Resolve ``agent.device_id`` to the node's one identity.

        The device-id file wins over ``ADOS_DEVICE_ID`` and over whatever
        config.yaml carries (``resolve_device_id``), so a stale or shortened
        configured id never shadows the provisioned one. When nothing resolves,
        the full id is minted and persisted at the device-id path so later
        validations and every other reader agree. The id is never truncated.
        """
        if isinstance(data, dict):
            agent = data.get("agent", {})
            if isinstance(agent, dict):
                from ados.core.identity import (
                    device_id_path,
                    get_or_create_device_id,
                    resolve_device_id,
                )

                # A YAML scalar of all digits loads as an int; the id is text.
                configured = str(agent.get("device_id") or "")
                agent["device_id"] = resolve_device_id(
                    configured
                ) or get_or_create_device_id(device_id_path())
                data["agent"] = agent
        return data
