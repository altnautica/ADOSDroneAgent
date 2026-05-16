"""System-level service configuration (OTA, vision, logging, pairing, discovery, ROS, swarm, UI)."""

from __future__ import annotations

from typing import Literal

from pydantic import BaseModel, field_validator

from ados.core.paths import FLIGHT_LOGS_DIR, PAIRING_JSON


class OtaConfig(BaseModel):
    channel: str = "stable"
    check_interval: int = 24
    auto_install: bool = False
    github_repo: str = "altnautica/ADOSDroneAgent"
    pip_path: str = "/opt/ados/venv/bin/pip"
    service_name: str = "ados-supervisor"


class VisionConfig(BaseModel):
    enabled: bool = False
    backend: str = "auto"  # auto, rknn, tensorrt, opencv_dnn, tflite
    confidence_threshold: float = 0.5
    models_dir: str = "/opt/ados/models/vision"
    models_cache_max_mb: int = 500
    registry_url: str = "https://raw.githubusercontent.com/altnautica/ADOSMissionControl/main/public/models/registry.json"
    auto_download: bool = True


class LoggingConfig(BaseModel):
    level: str = "info"
    max_size_mb: int = 50
    keep_count: int = 5
    flight_log_dir: str = str(FLIGHT_LOGS_DIR)


class PairingConfig(BaseModel):
    state_path: str = str(PAIRING_JSON)
    convex_url: str = ""  # Convex HTTP endpoint for cloud pairing
    beacon_interval: int = 30  # seconds
    heartbeat_interval: int = 60  # seconds
    single_process_cloud_enabled: bool = False
    code_ttl: int = 900  # 15 minutes
    # Cloud pair beacon publishes the unpaired agent's short-lived
    # pair code to ``convex_url`` so a GCS reached from any network
    # (e.g. command.altnautica.com) can claim by code. Loop runs only
    # while unpaired and gates on a non-empty ``convex_url`` — air-gap
    # operators get a clean opt-out by setting ``server.mode = "local"``
    # which clears the URL and stops every cloud-touching task at the
    # same gate.
    beacon_enabled: bool = True


class DiscoveryConfig(BaseModel):
    mdns_enabled: bool = True
    service_type: str = "_ados._tcp.local."


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
