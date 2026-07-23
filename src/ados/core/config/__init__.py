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
* ``radio.py`` — :class:`RadioConfig`, :class:`CrsfConfig`
* ``cloud.py`` — :class:`ServerConfig`, :class:`RemoteAccessConfig` and
  friends
* ``security.py`` — :class:`SecurityConfig` and friends, plus
  :data:`DEFAULT_CORS_ORIGINS`
* ``api.py`` — :class:`ApiConfig`, :class:`RestApiConfig`
* ``system.py`` — :class:`VisionConfig`,
  :class:`LoggingConfig`, :class:`PairingConfig`, :class:`DiscoveryConfig`,
  :class:`SwarmConfig`, :class:`LoraConfig`,
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
    _migrate_api_from_scripting,
    _migrate_gs_ui_from_legacy_json,
    _migrate_share_uplink_from_legacy_json,
)
from .agent import AgentConfig
from .api import ApiConfig, RestApiConfig
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
    KioskConfig,
    MeshConfig,
    WfbReceiverConfig,
    WfbRelayConfig,
)
from .mavlink import EndpointConfig, MavlinkConfig
from .network import (
    CellularConfig,
    HotspotConfig,
    NetworkConfig,
    RegulatoryConfig,
    WifiClientConfig,
)
from .radio import CrsfConfig, RadioConfig
from .root import SECRET_PATHS, ADOSConfig
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
    PairingConfig,
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
    "SECRET_PATHS",
    "load_config",
    # agent
    "AgentConfig",
    # api
    "ApiConfig",
    "RestApiConfig",
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
    "RegulatoryConfig",
    "WifiClientConfig",
    # radio
    "CrsfConfig",
    "RadioConfig",
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
    # system
    "DiscoveryConfig",
    "LoggingConfig",
    "LoraConfig",
    "PairingConfig",
    "SwarmConfig",
    "UiConfig",
    "VisionConfig",
    "WifiDirectConfig",
    # ground station
    "GroundStationConfig",
    "GroundStationUiConfig",
    "KioskConfig",
    "MeshConfig",
    "WfbReceiverConfig",
    "WfbRelayConfig",
]


class _StringTimestampLoader(yaml.SafeLoader):
    """A SafeLoader that keeps ISO-8601 timestamps as plain strings.

    The native config writers persist timestamps (e.g. ``video.wfb.paired_at``)
    as unquoted ISO-8601 values. The stock loader resolves those to ``datetime``,
    which then fails the str-typed config fields. Dropping the timestamp implicit
    resolver keeps every unquoted timestamp a string on the read side, so the
    YAML written by any process round-trips into the models unchanged.
    """


_StringTimestampLoader.yaml_implicit_resolvers = {
    first_char: [
        (tag, regexp)
        for tag, regexp in resolvers
        if tag != "tag:yaml.org,2002:timestamp"
    ]
    for first_char, resolvers in yaml.SafeLoader.yaml_implicit_resolvers.items()
}


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
                loaded = yaml.load(f, Loader=_StringTimestampLoader)
                if isinstance(loaded, dict):
                    raw = loaded
            picked_path = candidate
            break

    # Legacy migration: pull share_uplink out of the pre-package-split
    # side-file into the Pydantic-backed ground_station section. Idempotent,
    # runs at most once per process.
    raw = _migrate_share_uplink_from_legacy_json(raw, picked_path)
    raw = _migrate_gs_ui_from_legacy_json(raw, picked_path)
    # Relocate the REST-API surface config out of the legacy `scripting`
    # block into the dedicated `api` section. Idempotent, one-shot.
    raw = _migrate_api_from_scripting(raw, picked_path)

    # Load defaults.yaml from package data
    import importlib.resources
    defaults: dict[str, Any] = {}
    try:
        defaults_ref = importlib.resources.files("ados.core").joinpath("defaults.yaml")
        defaults_text = defaults_ref.read_text(encoding="utf-8")
        loaded = yaml.load(defaults_text, Loader=_StringTimestampLoader)
        if isinstance(loaded, dict):
            defaults = loaded
    except (FileNotFoundError, TypeError):
        pass

    merged = _deep_merge(defaults, raw)
    return ADOSConfig(**merged)
