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
  :class:`SwarmConfig`, :class:`SwarmFlockConfig`,
  :class:`SwarmSeparationConfig`, :class:`SwarmTasksConfig`,
  :class:`WifiDirectConfig`, :class:`UiConfig`
* ``ground_station.py`` — ground-station-profile-only models
* ``root.py`` — :class:`ADOSConfig` (top-level)
* ``_migrators.py`` — pure in-memory legacy normalisers + ``_deep_merge``
* ``_lock.py`` — the reader/writer flock shared with the native writers
* ``_yaml.py`` — the YAML loader that keeps ISO-8601 timestamps as strings
* ``maintenance.py`` — the off-startup-path migration pass that persists
  what ``_migrators`` normalises
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

import yaml

from ados.core.paths import CONFIG_YAML

from ._lock import shared_config_lock
from ._migrators import _deep_merge, apply_migrations
from ._yaml import StringTimestampLoader
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
    PairingConfig,
    SwarmConfig,
    SwarmFlockConfig,
    SwarmSeparationConfig,
    SwarmTasksConfig,
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
    "PairingConfig",
    "SwarmConfig",
    "SwarmFlockConfig",
    "SwarmSeparationConfig",
    "SwarmTasksConfig",
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


# Kept as a module-level alias so the loader has one name in the package.
_StringTimestampLoader = StringTimestampLoader

# One INFO line per process when a node is running un-migrated config, so an
# operator whose installer step never ran can see it. The read path applies
# every normalisation in memory, so this is a hygiene notice, not a fault.
_PENDING_LOGGED = False


def _note_pending(applied: list[str]) -> None:
    global _PENDING_LOGGED
    if not applied or _PENDING_LOGGED:
        return
    _PENDING_LOGGED = True
    # Plain logging, not `ados.core.logging`: that module calls
    # `load_config()`, so importing it here is a cycle.
    import logging as _logging

    _logging.getLogger("ados.core.config").info(
        "config normalised in memory; run `ados config migrate` to persist: "
        + ", ".join(applied)
    )


def load_config(path: str | Path | None = None) -> ADOSConfig:
    """Load config from YAML file, merging with defaults.

    Search order:
    1. Explicit path argument
    2. /etc/ados/config.yaml
    3. ./config.yaml
    4. Pure defaults (no file)

    **This function never writes.** It is on the startup path of every unit
    on the node, and they start concurrently; a read-modify-write here is
    eleven unsynchronised rewrites of the file that carries the radio
    pairing key, the profile and the role. Legacy shapes are normalised in
    memory by :func:`ados.core.config._migrators.apply_migrations` and
    persisted separately by :mod:`ados.core.config.maintenance`, which the
    installer runs.

    The read is taken under a shared :mod:`ados.core.config._lock` flock —
    the same lock the native writers take — on a bounded deadline, so it
    cannot interleave with a writer's read-modify-write window and cannot
    delay a unit's start if the lock is unavailable.
    """
    candidates: list[Path] = []
    if path:
        candidates.append(Path(path))
    candidates.extend([
        CONFIG_YAML,
        Path("config.yaml"),
    ])

    raw: dict[str, Any] = {}
    with shared_config_lock():
        for candidate in candidates:
            if candidate.is_file():
                with open(candidate) as f:
                    loaded = yaml.load(f, Loader=StringTimestampLoader)
                    if isinstance(loaded, dict):
                        raw = loaded
                break

    _note_pending(apply_migrations(raw))

    # Load defaults.yaml from package data
    import importlib.resources
    defaults: dict[str, Any] = {}
    try:
        defaults_ref = importlib.resources.files("ados.core").joinpath("defaults.yaml")
        defaults_text = defaults_ref.read_text(encoding="utf-8")
        loaded = yaml.load(defaults_text, Loader=StringTimestampLoader)
        if isinstance(loaded, dict):
            defaults = loaded
    except (FileNotFoundError, TypeError):
        pass

    merged = _deep_merge(defaults, raw)
    return ADOSConfig(**merged)
