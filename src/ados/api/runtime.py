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
    def model_manager(self) -> Any:
        return getattr(self._runtime, "model_manager", None)

    def health_dict(self) -> dict:
        return self._runtime.health.last.to_dict()

    def uptime_seconds(self) -> float:
        return self._runtime.uptime_seconds

    def _runtime_attr(self, public_name: str, private_name: str, default: Any = None) -> Any:
        if hasattr(self._runtime, public_name):
            return getattr(self._runtime, public_name)
        return getattr(self._runtime, private_name, default)

    def service_tasks(self) -> list[Any]:
        tasks = self._runtime_attr("service_task_handles", "_tasks", [])
        return list(tasks or [])

    def state_ipc_state(self) -> dict:
        state_client = self._runtime_attr("state_client", "_state_client")
        if state_client and state_client.state:
            return state_client.state
        return {}

    def fc_connection(self) -> Any:
        return self._runtime_attr("fc_connection_handle", "_fc_connection")

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
        return self._runtime_attr("vehicle_state", "_vehicle_state")

    def vehicle_state_dict(self) -> dict:
        state = self.vehicle_state()
        if state:
            return state.to_dict()
        # In the multi-process supervisor (production), the API service
        # has no in-process VehicleState. The mavlink service publishes
        # the live snapshot to `/run/ados/state.sock` at ~10 Hz and the
        # standalone runtime subscribes via the StateIPC client. Without
        # this fallback the REST `/api/telemetry` surface returns an
        # empty dict even while MAVLink frames are decoding correctly.
        ipc_state = self.state_ipc_state()
        if not ipc_state:
            return {}
        # The IPC payload also carries fc_connected / fc_port / fc_baud
        # / service_uptime alongside the vehicle keys. Strip those so
        # /api/telemetry surfaces only the vehicle state fields the GCS
        # expects (heartbeat, attitude, gps, battery, etc.).
        _ipc_only_keys = {
            "fc_connected",
            "fc_port",
            "fc_baud",
            "service_uptime",
        }
        return {k: v for k, v in ipc_state.items() if k not in _ipc_only_keys}

    def param_cache(self) -> Any:
        return self._runtime_attr("param_cache_handle", "_param_cache")

    def video_pipeline(self) -> Any:
        return self._runtime_attr("video_pipeline_handle", "_video_pipeline")

    def wfb_manager(self) -> Any:
        return self._runtime_attr("wfb_manager_handle", "_wfb_manager")

    def bitrate_controller(self) -> Any:
        return self._runtime_attr(
            "bitrate_controller_handle", "_bitrate_controller"
        )

    def scripting_handles(self) -> ScriptingHandles:
        return ScriptingHandles(
            runner=self._runtime_attr("script_runner", "_script_runner"),
            executor=self._runtime_attr("command_executor", "_command_executor"),
            demo=self._runtime_attr("demo_scripting", "_demo_scripting"),
        )

    def signing_observer(self) -> Any:
        return self._runtime_attr("signing_observer", "_signing_observer")


class StandaloneApiRuntime:
    """Runtime object used when the REST API runs as its own service."""

    def __init__(self, config: ADOSConfig, state_client: Any, log: Any) -> None:
        from ados.core.health import HealthMonitor
        from ados.core.pairing import PairingManager

        self.config = config
        self.state_client = state_client
        self.pairing_manager = PairingManager(state_path=config.pairing.state_path)
        self.services = ServiceTracker()
        self.service_task_handles: list[Any] = []
        self.fc_connection_handle = None
        self.vehicle_state = None
        self.param_cache_handle = None
        self.video_pipeline_handle = None
        self.wfb_manager_handle = None
        self.command_executor = None
        self.script_runner = None
        self.demo_scripting = None
        self.signing_observer = None
        self.ota_updater = None
        self.discovery_service = None
        self.board_name = "unknown"
        self.health = HealthMonitor()
        self.demo = False
        self.model_manager = None
        self._initialize_model_manager(log)

    @property
    def uptime_seconds(self) -> float:
        return 0.0

    def _initialize_model_manager(self, log: Any) -> None:
        try:
            from pathlib import Path

            import yaml

            from ados.hal.detect import detect_board
            from ados.services.vision.model_manager import ModelManager

            board_info = detect_board()
            self.board_name = board_info.name
            board_profile_dict: dict = {}
            boards_dir = Path(__file__).resolve().parent.parent / "hal" / "boards"
            if not boards_dir.exists():
                import ados.hal

                boards_dir = Path(ados.hal.__file__).parent / "boards"
            for yf in boards_dir.glob("*.yaml"):
                with open(yf) as f:
                    data = yaml.safe_load(f) or {}
                if data.get("name") == board_info.name:
                    board_profile_dict = data
                    break
            npu_tops = board_profile_dict.get("compute", {}).get("npu_tops", 0)
            self.model_manager = ModelManager(self.config.vision, npu_tops=npu_tops)
            log.info("model_manager_initialized", board=board_info.name, npu_tops=npu_tops)
        except Exception as e:
            log.warning("model_manager_init_failed", error=str(e))


def ensure_api_runtime(runtime: ApiRuntime | ApiRuntimeFacade | Any) -> ApiRuntimeFacade:
    """Return an API runtime facade, wrapping raw runtimes once."""
    if isinstance(runtime, ApiRuntimeFacade):
        return runtime
    return ApiRuntimeFacade(runtime)
