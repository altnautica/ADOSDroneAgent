"""Runtime contract consumed by the REST API layer."""

from __future__ import annotations

from typing import Any, Protocol

from ados.core.config import ADOSConfig
from ados.core.service_tracker import ServiceTracker


class ApiRuntime(Protocol):
    """Minimal runtime surface required by API routes."""

    config: ADOSConfig
    services: ServiceTracker
    health: Any
    pairing_manager: Any
    discovery_service: Any
    board_name: str
    demo: bool
    ota_updater: Any
    feature_manager: Any
    model_manager: Any
    _tasks: list[Any]
    _fc_connection: Any
    _vehicle_state: Any
    _param_cache: Any
    _video_pipeline: Any
    _wfb_manager: Any
    _command_executor: Any
    _script_runner: Any
    _demo_scripting: Any

    @property
    def uptime_seconds(self) -> float:
        """Return process uptime in seconds."""
