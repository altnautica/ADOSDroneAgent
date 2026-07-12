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


class ADOSConfig(BaseModel):
    agent: AgentConfig = AgentConfig()
    mavlink: MavlinkConfig = MavlinkConfig()
    video: VideoConfig = VideoConfig()
    network: NetworkConfig = NetworkConfig()
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
