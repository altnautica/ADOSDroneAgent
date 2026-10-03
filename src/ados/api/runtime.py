"""Runtime facade consumed by the REST API layer.

The residual API runs as its own service (``ados.services.api``). Everything
live it reports comes from outside this process: the vehicle snapshot the
native router publishes on the state socket, the config on disk, the pairing
state file. The facade names exactly those sources, so a route cannot reach for
an in-process handle that does not exist and quietly report its default.
"""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Protocol

from ados.core.config import ADOSConfig, load_config
from ados.core.config.writer import ConfigWriteResult, set_config_values
from ados.core.paths import CONFIG_YAML


class ApiRuntime(Protocol):
    """Raw runtime object accepted by the API server."""

    config: ADOSConfig
    pairing_manager: Any
    state_client: Any
    board_name: str
    model_manager: Any

    def write_config(self, values: Mapping[str, Any]) -> ConfigWriteResult:
        """Set the dotted-path leaves in ``values`` on the config document."""


@dataclass(frozen=True)
class FcStatus:
    """Flight-controller status assembled from the state IPC snapshot."""

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


# The state snapshot carries the link keys alongside the vehicle keys; the
# vehicle view strips them so it holds only what the GCS reads as telemetry.
_IPC_LINK_KEYS = frozenset({"fc_connected", "fc_port", "fc_baud", "service_uptime"})


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
    def pairing_manager(self) -> Any:
        return self._runtime.pairing_manager

    @property
    def board_name(self) -> str:
        return self._runtime.board_name

    @property
    def board_soc(self) -> str | None:
        """The detected SoC (``rk3588``), or ``None`` when detection did not say."""
        return getattr(self._runtime, "board_soc", None)

    @property
    def board_model(self) -> str | None:
        """The device-tree model string, or ``None`` when detection did not say."""
        return getattr(self._runtime, "board_model", None)

    @property
    def model_manager(self) -> Any:
        return getattr(self._runtime, "model_manager", None)

    def write_config(self, values: Mapping[str, Any]) -> ConfigWriteResult:
        """Set dotted-path config leaves (``{"server.mode": "local"}``) on disk.

        A runtime that exposes no `write_config`, or one whose write raised, is
        reported as a failed write with the reason attached — never as a bare
        False that leaves the route with nothing to tell the operator.
        """
        writer = getattr(self._runtime, "write_config", None)
        if not callable(writer):
            return ConfigWriteResult(
                ok=False,
                error="this runtime cannot persist config (no write_config)",
            )
        try:
            result: ConfigWriteResult = writer(dict(values))
            return result
        except Exception as exc:  # noqa: BLE001 — surfaced as persist_error
            return ConfigWriteResult(ok=False, error=str(exc))

    def state_ipc_state(self) -> dict:
        state_client = getattr(self._runtime, "state_client", None)
        if state_client and state_client.state:
            return state_client.state
        return {}

    def fc_status(self) -> FcStatus:
        # The router publishes the FC identity + gated-truth siblings alongside
        # fc_connected, so an MSP FC (which never sets fc_connected) is still
        # honestly described by transport_open + fc_variant.
        state = self.state_ipc_state()
        fc_variant = state.get("fc_variant")
        fc_firmware = state.get("fc_firmware")
        fc_link_hint = state.get("fc_link_hint")
        return FcStatus(
            connected=bool(state.get("fc_connected")),
            port=state.get("fc_port"),
            baud=state.get("fc_baud"),
            uptime_seconds=state.get("service_uptime"),
            transport_open=bool(state.get("transport_open", False)),
            mavlink_alive=bool(state.get("mavlink_alive", False)),
            fc_variant=(str(fc_variant) if fc_variant else None),
            fc_firmware=(
                str(fc_firmware)
                if fc_firmware and fc_firmware != "unknown"
                else None
            ),
            fc_link_hint=(str(fc_link_hint) if fc_link_hint else None),
        )

    def vehicle_state_dict(self) -> dict:
        """The vehicle snapshot the native router publishes, link keys removed."""
        return {
            k: v
            for k, v in self.state_ipc_state().items()
            if k not in _IPC_LINK_KEYS
        }


class StandaloneApiRuntime:
    """Runtime object used when the REST API runs as its own service.

    The config document is co-written by the native routes and the CLI while
    this process runs, so ``config`` is re-read whenever the file changes and a
    write names only the leaves it sets. Nothing held in memory is ever written
    back over the file.
    """

    def __init__(
        self,
        config: ADOSConfig,
        state_client: Any,
        log: Any,
        *,
        config_path: Path = CONFIG_YAML,
    ) -> None:
        from ados.core.pairing import PairingManager

        self._config_path = Path(config_path)
        self._config = config
        self._config_stamp = self._stat_stamp()
        self._log = log
        self.state_client = state_client
        self.pairing_manager = PairingManager(state_path=config.pairing.state_path)
        self.board_name = "unknown"
        self.board_soc: str | None = None
        self.board_model: str | None = None
        self.model_manager = None
        self._initialize_model_manager(log)

    def _stat_stamp(self) -> tuple[int, int, int] | None:
        # An atomic replace gives the file a new inode, so a rewrite is seen
        # even when the mtime tick and the size happen to match the last read.
        try:
            st = self._config_path.stat()
        except OSError:
            return None
        return (st.st_ino, st.st_mtime_ns, st.st_size)

    @property
    def config(self) -> ADOSConfig:
        """The config as the file says it now, re-read when the file changed."""
        stamp = self._stat_stamp()
        if stamp != self._config_stamp:
            try:
                self._config = load_config(self._config_path)
                self._config_stamp = stamp
            except Exception as exc:  # noqa: BLE001 — keep serving the last good read
                self._log.warning("config_reload_failed", error=str(exc))
        return self._config

    def write_config(self, values: Mapping[str, Any]) -> ConfigWriteResult:
        """Set dotted-path leaves on the config document, under the write lock.

        Only the named leaves change; every other key, including those the
        Python models do not declare and those another writer changed since
        this process started, round-trips verbatim. The resulting document
        must still validate, so an out-of-range value is refused rather than
        persisted. The next ``config`` read sees the write.
        """
        return set_config_values(values, path=self._config_path, model=ADOSConfig)

    def _initialize_model_manager(self, log: Any) -> None:
        try:
            from ados.hal.detect import detect_board
            from ados.services.vision.model_manager import ModelManager

            board_info = detect_board()
            self.board_name = board_info.name
            self.board_soc = board_info.soc if board_info.soc != "unknown" else None
            self.board_model = board_info.model or None
            # The board fingerprint sidecar (/run/ados/board.json) is written by
            # the supervisor at startup, in Rust, and has exactly one writer.
            # This process is not it. The NPU rating comes from the profile
            # detection resolved (override, compatible token, variant), never
            # from a second match over the board YAMLs.
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
