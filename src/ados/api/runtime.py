"""Runtime facade consumed by the REST API layer."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Protocol

from ados.core.config import ADOSConfig
from ados.core.config.writer import ConfigWriteResult, persist_config_model
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
    model_manager: Any

    @property
    def uptime_seconds(self) -> float:
        """Return process uptime in seconds."""


@dataclass(frozen=True)
class FcStatus:
    """Flight-controller status assembled from IPC or in-process state."""

    connected: bool
    port: Any = None
    baud: Any = None
    uptime_seconds: float | None = None
    transport_open: bool = False
    mavlink_alive: bool = False
    fc_variant: str | None = None
    fc_firmware: str | None = None
    fc_link_hint: str | None = None

    @property
    def reachable(self) -> bool:
        """Honest FC reachability, mirroring the Rust /api/status derive_fc_reachable.

        True for a live MAVLink link OR an identified/MSP FC on an open transport.
        An MSP FC (Betaflight/iNav) is reachable and drivable even though it never
        emits a MAVLink heartbeat, so it must not read as a broken link.
        """
        if self.mavlink_alive:
            return True
        if not self.transport_open:
            return False
        return bool(self.fc_variant) or self.fc_link_hint == "msp_detected"


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
    def model_manager(self) -> Any:
        return getattr(self._runtime, "model_manager", None)

    def save_config(self) -> ConfigWriteResult:
        """Persist the underlying runtime's config to disk.

        Delegates to `runtime.save_config()` when available. The legacy
        `_gs._save_config(app)` helper does `getattr(app, "save_config", None)`
        with the facade as `app`; surfacing the method on the facade keeps
        every historical callsite working.

        A runtime that exposes no `save_config`, or one whose save raised, is
        reported as a failed write with the reason attached — never as a bare
        False that leaves the route with nothing to tell the operator.
        """
        saver = getattr(self._runtime, "save_config", None)
        if not callable(saver):
            return ConfigWriteResult(
                ok=False,
                error="this runtime cannot persist config (no save_config)",
            )
        try:
            result = saver()
        except Exception as exc:  # noqa: BLE001 — surfaced as persist_error
            return ConfigWriteResult(ok=False, error=str(exc))
        if isinstance(result, ConfigWriteResult):
            return result
        # A test double or an alternative runtime may still answer with a
        # plain truthiness. Honour it rather than calling its write failed.
        return ConfigWriteResult(ok=bool(result) or result is None)

    def health_dict(self) -> dict:
        # Refresh the sample before serializing. The standalone API
        # service does not run the supervisor loop that periodically
        # calls check_system(), so without this the heartbeat would
        # forever return the default zero-valued SystemHealth().
        try:
            self._runtime.health.check_system()
        except Exception:
            pass
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
        # The router publishes the FC identity + gated-truth siblings alongside
        # fc_connected, so an MSP FC (which never sets fc_connected) is still
        # honestly described by transport_open + fc_variant.
        transport_open = state.get("transport_open", False)
        mavlink_alive = state.get("mavlink_alive", False)
        fc_variant = state.get("fc_variant")
        fc_firmware = state.get("fc_firmware")
        fc_link_hint = state.get("fc_link_hint")

        # Prefer the live connection's truth when a handle is present.
        # The IPC snapshot can lag a physical FC unplug (it only changes
        # on the next state write), so a cached `fc_connected: True`
        # would otherwise keep reporting the FC as connected after it is
        # gone. The live handle reflects the actual link state now. When
        # no live handle exists (the standalone API service) the IPC
        # snapshot remains the only source.
        fc = self.fc_connection()
        if fc is not None:
            connected = getattr(fc, "connected", False)
            port = getattr(fc, "port", None)
            baud = getattr(fc, "baud", None)
            transport_open = getattr(fc, "transport_open", transport_open)
            mavlink_alive = getattr(fc, "mavlink_alive", mavlink_alive)
            fc_variant = getattr(fc, "fc_variant", fc_variant)
            fc_firmware = getattr(fc, "fc_firmware", fc_firmware)
            fc_link_hint = getattr(fc, "fc_link_hint", fc_link_hint)

        return FcStatus(
            connected=bool(connected),
            port=port,
            baud=baud,
            uptime_seconds=uptime,
            transport_open=bool(transport_open),
            mavlink_alive=bool(mavlink_alive),
            fc_variant=(str(fc_variant) if fc_variant else None),
            fc_firmware=(
                str(fc_firmware)
                if fc_firmware and fc_firmware != "unknown"
                else None
            ),
            fc_link_hint=(str(fc_link_hint) if fc_link_hint else None),
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
        self.signing_observer = None
        self.discovery_service = None
        self.board_name = "unknown"
        self.health = HealthMonitor()
        self.demo = False
        self.model_manager = None
        self._initialize_model_manager(log)

    @property
    def uptime_seconds(self) -> float:
        return 0.0

    def save_config(self) -> ConfigWriteResult:
        """Persist the fields a caller changed on `self.config`.

        Delegates to the one config writer, which merges the changed leaves
        into the on-disk document under the write lock. Two properties matter
        to every caller of this method:

        * A key this Python model does not declare — `mavlink.injector_arbitration`
          (the FC-write arbiter), `network.watchdog.enabled` (the SoC watchdog),
          `agent.headless`, `video.wfb.reg_gate_strict` — survives the write
          verbatim. The previous implementation serialised `model_dump()` over
          the whole file and deleted all of them.
        * A field nobody touched is not written, so a node keeps tracking the
          shipped default instead of freezing this release's value.

        The result is truthy on success, so `bool(app.save_config())` reads
        correctly; a caller that owes the operator a reason reads
        `result.error`, which is never None on failure.
        """
        return persist_config_model(self.config)

    def _initialize_model_manager(self, log: Any) -> None:
        try:
            from ados.hal.detect import detect_board
            from ados.services.vision.model_manager import ModelManager

            board_info = detect_board()
            self.board_name = board_info.name
            # The board fingerprint sidecar (/run/ados/board.json) is written by
            # the supervisor at startup, in Rust, and has exactly one writer.
            # This process is not it: a second writer of the same document is how
            # a zero-Python node ended up with no writer at all. The NPU rating
            # comes from the profile detection resolved (override, compatible
            # token, variant), never from a second match over the board YAMLs.
            npu_tops = board_info.npu_tops
            self.model_manager = ModelManager(self.config.vision, npu_tops=npu_tops)
            log.info("model_manager_initialized", board=board_info.name, npu_tops=npu_tops)
        except Exception as e:
            log.warning("model_manager_init_failed", error=str(e))


def ensure_api_runtime(runtime: ApiRuntime | ApiRuntimeFacade | Any) -> ApiRuntimeFacade:
    """Return an API runtime facade, wrapping raw runtimes once."""
    if isinstance(runtime, ApiRuntimeFacade):
        return runtime
    return ApiRuntimeFacade(runtime)
