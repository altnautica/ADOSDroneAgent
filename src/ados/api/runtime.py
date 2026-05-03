"""Runtime facade consumed by the REST API layer."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Protocol

from ados.core.config import ADOSConfig
from ados.core.service_tracker import ServiceTracker


class ApiRuntime(Protocol):
    """Raw runtime object accepted by the API server."""

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

    @property
    def uptime_seconds(self) -> float:
        """Return process uptime in seconds."""


@dataclass(frozen=True)
class ScriptingHandles:
    """Runtime handles for scripting routes."""

    runner: Any
    executor: Any
    demo: Any


@dataclass(frozen=True)
class FcStatus:
    """Flight-controller status assembled from IPC or in-process state."""

    connected: bool
    port: Any = None
    baud: Any = None
    uptime_seconds: float | None = None


class ApiRuntimeFacade:
    """Named API-facing accessors over the agent runtime implementation."""

    def __init__(self, runtime: ApiRuntime | Any) -> None:
        self._runtime = runtime

    @property
    def raw_runtime(self) -> Any:
        """Return the wrapped runtime for integration points not yet narrowed."""
        return self._runtime

    @property
    def config(self) -> ADOSConfig:
        return self._runtime.config

    @property
    def service_tracker(self) -> ServiceTracker:
        return self._runtime.services

    @property
    def services(self) -> ServiceTracker:
        return self.service_tracker

    @property
    def pairing_manager(self) -> Any:
        return self._runtime.pairing_manager

    @property
    def discovery_service(self) -> Any:
        return self._runtime.discovery_service

    @property
    def board_name(self) -> str:
        return self._runtime.board_name

    @property
    def demo(self) -> bool:
        return self._runtime.demo

    @property
    def ota_updater(self) -> Any:
        return getattr(self._runtime, "ota_updater", None)

    @property
    def feature_manager(self) -> Any:
        return getattr(self._runtime, "feature_manager", None)

    @property
    def model_manager(self) -> Any:
        return getattr(self._runtime, "model_manager", None)

    def health_dict(self) -> dict:
        return self._runtime.health.last.to_dict()

    def uptime_seconds(self) -> float:
        return self._runtime.uptime_seconds

    def service_tasks(self) -> list[Any]:
        return list(getattr(self._runtime, "_tasks", []))

    def state_ipc_state(self) -> dict:
        state_client = getattr(self._runtime, "_state_client", None)
        if state_client and state_client.state:
            return state_client.state
        return {}

    def fc_connection(self) -> Any:
        return getattr(self._runtime, "_fc_connection", None)

    def fc_status(self) -> FcStatus:
        state = self.state_ipc_state()
        connected = state.get("fc_connected")
        port = state.get("fc_port")
        baud = state.get("fc_baud")
        uptime = state.get("service_uptime")

        fc = self.fc_connection()
        if connected is None and fc is not None:
            connected = getattr(fc, "connected", False)
            port = getattr(fc, "port", None)
            baud = getattr(fc, "baud", None)

        return FcStatus(
            connected=bool(connected),
            port=port,
            baud=baud,
            uptime_seconds=uptime,
        )

    def vehicle_state(self) -> Any:
        return getattr(self._runtime, "_vehicle_state", None)

    def vehicle_state_dict(self) -> dict:
        state = self.vehicle_state()
        if state:
            return state.to_dict()
        return {}

    def param_cache(self) -> Any:
        return getattr(self._runtime, "_param_cache", None)

    def video_pipeline(self) -> Any:
        return getattr(self._runtime, "_video_pipeline", None)

    def wfb_manager(self) -> Any:
        return getattr(self._runtime, "_wfb_manager", None)

    def scripting_handles(self) -> ScriptingHandles:
        return ScriptingHandles(
            runner=getattr(self._runtime, "_script_runner", None),
            executor=getattr(self._runtime, "_command_executor", None),
            demo=getattr(self._runtime, "_demo_scripting", None),
        )

    def signing_observer(self) -> Any:
        return getattr(self._runtime, "_signing_observer", None)


def ensure_api_runtime(runtime: ApiRuntime | ApiRuntimeFacade | Any) -> ApiRuntimeFacade:
    """Return an API runtime facade, wrapping raw runtimes once."""
    if isinstance(runtime, ApiRuntimeFacade):
        return runtime
    return ApiRuntimeFacade(runtime)
