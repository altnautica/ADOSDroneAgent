"""Configuration models and loader for ADOS Drone Agent.

This package re-exports every public name that used to live in the single
``ados/core/config.py`` module, so existing callers (``from ados.core.config
import X``) keep working unchanged. The implementation now lives in
per-domain files alongside this barrel:

* ``agent.py`` — :class:`AgentConfig`
* ``mavlink.py`` — :class:`MavlinkConfig`, :class:`EndpointConfig`
* ``wfb.py`` — :class:`WfbConfig`
* ``video.py`` — :class:`VideoConfig`, :class:`CameraConfig`,
  :class:`RecordingConfig`
* ``network.py`` — :class:`NetworkConfig` and friends
* ``cloud.py`` — :class:`ServerConfig`, :class:`RemoteAccessConfig` and
  friends
* ``security.py`` — :class:`SecurityConfig` and friends, plus
  :data:`DEFAULT_CORS_ORIGINS`
* ``scripting.py`` — :class:`ScriptingConfig` and friends
* ``system.py`` — :class:`OtaConfig`, :class:`VisionConfig`,
  :class:`LoggingConfig`, :class:`PairingConfig`, :class:`DiscoveryConfig`,
  :class:`RosConfig`, :class:`SwarmConfig`, :class:`LoraConfig`,
  :class:`WifiDirectConfig`, :class:`UiConfig`
* ``ground_station.py`` — ground-station-profile-only models
* ``root.py`` — :class:`ADOSConfig` (top-level)
* ``_migrators.py`` — legacy side-file migrators + ``_deep_merge``
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

import yaml

from ados.core.paths import CONFIG_YAML

from ._migrators import (
    _deep_merge,
    _migrate_gs_ui_from_legacy_json,
    _migrate_share_uplink_from_legacy_json,
)
from .agent import AgentConfig
from .cloud import (
    CloudflareTunnelConfig,
    CloudServerConfig,
    RemoteAccessConfig,
    SelfHostedServerConfig,
    ServerConfig,
)
from .ground_station import (
    GroundStationConfig,
    GroundStationUiConfig,
    MeshConfig,
    WfbReceiverConfig,
    WfbRelayConfig,
)
from .mavlink import EndpointConfig, MavlinkConfig
from .network import (
    CellularConfig,
    HotspotConfig,
    NetworkConfig,
    WifiClientConfig,
)
from .root import ADOSConfig
from .scripting import (
    RestApiConfig,
    ScriptingConfig,
    ScriptsConfig,
    SuiteConfig,
    TextCommandsConfig,
)
from .security import (
    DEFAULT_CORS_ORIGINS,
    ApiSecurityConfig,
    SecurityConfig,
    TlsConfig,
    WireguardConfig,
)
from .system import (
    DiscoveryConfig,
    LoggingConfig,
    LoraConfig,
    OtaConfig,
    PairingConfig,
    RosConfig,
    SwarmConfig,
    UiConfig,
    VisionConfig,
    WifiDirectConfig,
)
from .video import CameraConfig, RecordingConfig, VideoConfig
from .wfb import WfbConfig

__all__ = [
    # root
    "ADOSConfig",
    "load_config",
    # agent
    "AgentConfig",
    # mavlink
    "EndpointConfig",
    "MavlinkConfig",
    # wfb
    "WfbConfig",
    # video
    "CameraConfig",
    "RecordingConfig",
    "VideoConfig",
    # network
    "CellularConfig",
    "HotspotConfig",
    "NetworkConfig",
    "WifiClientConfig",
    # cloud
    "CloudServerConfig",
    "CloudflareTunnelConfig",
    "RemoteAccessConfig",
    "SelfHostedServerConfig",
    "ServerConfig",
    # security
    "ApiSecurityConfig",
    "DEFAULT_CORS_ORIGINS",
    "SecurityConfig",
    "TlsConfig",
    "WireguardConfig",
    # scripting
    "RestApiConfig",
    "ScriptingConfig",
    "ScriptsConfig",
    "SuiteConfig",
    "TextCommandsConfig",
    # system
    "DiscoveryConfig",
    "LoggingConfig",
    "LoraConfig",
    "OtaConfig",
    "PairingConfig",
    "RosConfig",
    "SwarmConfig",
    "UiConfig",
    "VisionConfig",
    "WifiDirectConfig",
    # ground station
    "GroundStationConfig",
    "GroundStationUiConfig",
    "MeshConfig",
    "WfbReceiverConfig",
    "WfbRelayConfig",
]


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

    # Legacy migration: pull share_uplink out of the pre-package-split
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
