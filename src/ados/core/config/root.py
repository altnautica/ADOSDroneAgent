"""Root ADOSConfig model — aggregates every domain section."""

from __future__ import annotations

from typing import Any

from pydantic import BaseModel, Field, model_validator

from .agent import AgentConfig
from .cloud import RemoteAccessConfig, ServerConfig
from .ground_station import GroundStationConfig
from .mavlink import MavlinkConfig
from .network import NetworkConfig
from .scripting import ScriptingConfig
from .security import SecurityConfig
from .system import (
    DiscoveryConfig,
    LoggingConfig,
    OtaConfig,
    PairingConfig,
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
    scripting: ScriptingConfig = ScriptingConfig()
    ota: OtaConfig = OtaConfig()
    logging: LoggingConfig = LoggingConfig()
    pairing: PairingConfig = PairingConfig()
    discovery: DiscoveryConfig = DiscoveryConfig()
    vision: VisionConfig = VisionConfig()
    swarm: SwarmConfig = SwarmConfig()
    ground_station: GroundStationConfig = GroundStationConfig()
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
