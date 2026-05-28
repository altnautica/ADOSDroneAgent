"""Test helpers for building API runtime doubles."""

from __future__ import annotations

from typing import Any
from unittest.mock import MagicMock

from ados.core.config import ADOSConfig
from ados.core.health import HealthMonitor
from ados.core.service_tracker import ServiceTracker
from ados.services.mavlink.state import VehicleState


class ApiRuntimeTestDouble:
    """Small runtime object shaped like the API facade contract."""

    def __init__(
        self,
        *,
        config: ADOSConfig | None = None,
        uptime_seconds: float = 42.0,
        vehicle_state: Any | None = None,
        fc_connection: Any | None = None,
        param_cache: Any | None = None,
        video_pipeline: Any | None = None,
        wfb_manager: Any | None = None,
        demo_scripting: Any | None = None,
        script_runner: Any | None = None,
        command_executor: Any | None = None,
        ota_updater: Any | None = None,
    ) -> None:
        self.config = config or ADOSConfig()
        self.health = HealthMonitor()
        self.services = ServiceTracker()
        self._uptime_seconds = uptime_seconds
        self.state_client = None
        self.service_task_handles: list[Any] = []
        self.vehicle_state = vehicle_state if vehicle_state is not None else VehicleState()
        self.fc_connection_handle = fc_connection or disconnected_fc_connection()
        self.param_cache_handle = param_cache
        self.video_pipeline_handle = video_pipeline
        self.wfb_manager_handle = wfb_manager
        self.demo_scripting = demo_scripting
        self.script_runner = script_runner
        self.command_executor = command_executor
        self.signing_observer = None
        self.ota_updater = ota_updater
        self.discovery_service = None
        self.board_name = "test"
        self.demo = False
        self.model_manager = None
        self.pairing_manager = MagicMock()
        self.pairing_manager.is_paired = False
        # The setup-status builder reads the live pairing code when the
        # agent is unpaired; the real PairingManager returns a 6-char
        # string here, so the double must too or model validation fails.
        self.pairing_manager.get_or_create_code.return_value = "ABC234"
        self.raw_runtime = _RawRuntimeStub()

    @property
    def uptime_seconds(self) -> float:
        return self._uptime_seconds


class _RawRuntimeStub:
    """Minimal stand-in for the agent runtime wrapper.

    Setup setters call ``runtime.raw_runtime.save_config()`` to persist
    config changes. The double ships a no-op so the route-level tests
    can drive the full apply flow without touching the filesystem.
    """

    def save_config(self) -> None:
        return None


def disconnected_fc_connection() -> MagicMock:
    fc_connection = MagicMock()
    fc_connection.connected = False
    fc_connection.port = ""
    fc_connection.baud = 0
    return fc_connection


def build_api_runtime(**kwargs: Any) -> ApiRuntimeTestDouble:
    return ApiRuntimeTestDouble(**kwargs)
