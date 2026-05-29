"""PluginContext and its capability-gated facades.

This is the public surface plugin authors program against. Each
property on :class:`PluginContext` is a thin facade backed by an IPC
call to the supervisor. Capability checks happen on the supervisor
side; the facades just shape arguments and decode responses.

Separated from :mod:`ados.plugins.ipc_client` to keep that module's
size budget reasonable. ``ipc_client`` imports from here and
re-exports the public names so existing imports keep working.
"""

from __future__ import annotations

import fnmatch
from collections.abc import Awaitable, Callable
from pathlib import Path
from typing import TYPE_CHECKING, Any

from ados.core.logging import get_logger

if TYPE_CHECKING:
    from ados.plugins.ipc_client import PluginIpcClient
    from ados.sdk.vision import VisionClient


def _matches(pattern: str, topic: str) -> bool:
    """Glob match on the topic. Wildcards behave like ``mavlink.*``."""
    return fnmatch.fnmatchcase(topic, pattern)


# ---------------------------------------------------------------------
# Facades
# ---------------------------------------------------------------------


class _EventsClient:
    def __init__(self, ipc: PluginIpcClient) -> None:
        self._ipc = ipc

    async def publish(self, topic: str, payload: dict | None = None) -> int:
        return await self._ipc.event_publish(topic, payload or {})

    async def subscribe(
        self,
        topic_pattern: str,
        callback: Callable[[dict], Awaitable[None] | None],
    ) -> None:
        await self._ipc.event_subscribe(topic_pattern, callback)


class _MAVLinkClient:
    """``ctx.mavlink`` facade.

    Wraps the supervisor's MAVLink router through the IPC bridge. The
    v1.0 hand-injected ``RouterHandle`` pattern (used by the gimbal
    plugin to send CommandLong / CommandInt directly) is retired in
    favor of ``ctx.mavlink.send``; the runner exposes a back-compat
    shim that adapts the old shape into this surface so existing
    plugins keep working without source changes.
    """

    def __init__(self, ipc: PluginIpcClient) -> None:
        self._ipc = ipc

    async def send(
        self, msg_bytes: bytes, component_id: int | None = None
    ) -> dict:
        return await self._ipc.mavlink_send(
            msg_bytes, component_id=component_id
        )

    async def subscribe(
        self,
        msg_name: str,
        callback: Callable[[dict], Awaitable[None] | None],
    ) -> None:
        await self._ipc.mavlink_subscribe(msg_name, callback)

    async def register_component(self, comp_id: int, kind: str) -> dict:
        return await self._ipc.mavlink_register_component(comp_id, kind)


def _driver_ref(driver: Any) -> str:
    """Map a driver instance to a short opaque reference the IPC carries.

    The host doesn't see the live Python object; it just records the
    plugin's claim. The actual frame pump or command-emit loop runs
    in the plugin process address space.
    """
    try:
        return getattr(driver, "driver_id", None) or type(driver).__name__
    except Exception:  # noqa: BLE001
        return "driver"


class _PeripheralManagerClient:
    """``ctx.peripheral_manager`` facade.

    Exposes register_*_driver for the six driver kinds (camera, lidar,
    gimbal, gps, esc, payload-actuator) plus camera-path claim. Each
    register call routes the driver instance's id back to the host's
    driver registry; the driver itself keeps running in the plugin
    address space.
    """

    def __init__(self, ipc: PluginIpcClient) -> None:
        self._ipc = ipc

    async def register_camera_driver(self, driver: Any) -> dict:
        return await self._ipc.peripheral_register_driver(
            "camera", _driver_ref(driver)
        )

    async def register_lidar_driver(self, driver: Any) -> dict:
        return await self._ipc.peripheral_register_driver(
            "lidar", _driver_ref(driver)
        )

    async def register_gimbal_driver(self, driver: Any) -> dict:
        return await self._ipc.peripheral_register_driver(
            "gimbal", _driver_ref(driver)
        )

    async def register_gps_driver(self, driver: Any) -> dict:
        return await self._ipc.peripheral_register_driver(
            "gps", _driver_ref(driver)
        )

    async def register_esc_driver(self, driver: Any) -> dict:
        return await self._ipc.peripheral_register_driver(
            "esc", _driver_ref(driver)
        )

    async def register_payload_actuator_driver(self, driver: Any) -> dict:
        return await self._ipc.peripheral_register_driver(
            "payload-actuator", _driver_ref(driver)
        )

    async def unregister(self, handle_id: str) -> dict:
        return await self._ipc.peripheral_unregister_driver(handle_id)

    async def unregister_camera_driver(self, driver: Any) -> None:
        # Legacy synchronous-looking shape used by v1.0 thermal-camera
        # plugin. The handle id is not returned because v1.0 callers
        # do not retain one; we tag by driver_ref so the host can find
        # the matching install. The supervisor records the absence as
        # a best-effort release on the plugin's next disconnect.
        ref = _driver_ref(driver)
        # No-op on the supervisor side until v1.1 GCS exposes
        # explicit handle ids. The release_plugin path on disconnect
        # cleans up regardless.
        _ = ref

    async def claim_camera(
        self, device_path: str, exclusive: bool = True
    ) -> dict:
        return await self._ipc.camera_claim(device_path, exclusive)


class _CameraClient:
    """``ctx.camera`` facade.

    Provides path-level claim/release plus a frame-pull primitive that
    vision plugins consume. The supervisor mediates which plugin holds
    exclusive ownership of a ``/dev/videoN`` path so a second plugin
    requesting exclusive on the same path is refused before any V4L2
    handle is opened.

    The ``get_frame`` method is the building block for vision behaviors
    that run as plugins (Follow Me, ActiveTrack, Precision Landing).
    It returns the latest frame on the supervisor's behalf — the
    plugin polls at its own desired rate and the supervisor enforces
    a host-side cap so a runaway plugin cannot DOS the camera pipeline.
    """

    def __init__(self, ipc: PluginIpcClient) -> None:
        self._ipc = ipc

    async def claim(self, device_path: str, exclusive: bool = True) -> dict:
        return await self._ipc.camera_claim(device_path, exclusive)

    async def release(self, device_path: str) -> dict:
        return await self._ipc.camera_release(device_path)

    async def get_frame(
        self,
        device_path: str,
        *,
        format: str = "nv12",
        timeout_ms: int = 1000,
    ) -> dict:
        """Return the latest captured frame from ``device_path``.

        Returns a dict shaped as::

            {
              "frame_id": int,
              "width": int,
              "height": int,
              "format": "nv12" | "rgb888" | ...,
              "data": bytes,
              "ts_ns": int,
              "stale": bool,
            }

        ``stale`` is True when the supervisor returns the previously
        captured frame because no new frame arrived within
        ``timeout_ms``. A plugin should treat repeated stale frames as
        a tracker-loss signal (the camera or capture pipeline stalled).

        Raises ``RpcError`` when the device is not claimed by this
        plugin or the format is unsupported.
        """
        return await self._ipc.camera_get_frame(
            device_path, format=format, timeout_ms=timeout_ms
        )


class _TelemetryClient:
    def __init__(self, ipc: PluginIpcClient) -> None:
        self._ipc = ipc

    async def extend(self, channel: str, payload: dict) -> dict:
        return await self._ipc.telemetry_extend(channel, payload)


class _ConfigClient:
    """Live config kv with per-drone or global scope.

    Read order: drone scope (when bound) -> global -> default.
    """

    def __init__(self, ipc: PluginIpcClient, static_config: dict) -> None:
        self._ipc = ipc
        self._static = dict(static_config or {})

    def static(self, key: str, default: Any = None) -> Any:
        """Read the manifest-supplied config dict; synchronous."""
        return self._static.get(key, default)

    async def get(self, key: str, default: Any = None) -> Any:
        return await self._ipc.config_get(key, default=default)

    async def set(self, key: str, value: Any, scope: str = "drone") -> dict:
        return await self._ipc.config_set(key, value, scope=scope)


class _ProcessClient:
    def __init__(self, ipc: PluginIpcClient) -> None:
        self._ipc = ipc

    async def spawn(
        self,
        basename: str,
        args: list[str] | None = None,
        env: dict[str, str] | None = None,
    ) -> dict:
        return await self._ipc.process_spawn(basename, args=args, env=env)


class _LifecycleClient:
    """Subscribe to GCS-side mount events.

    ``on_pause`` fires when the operator switches away from the drone
    whose detail panel hosts this plugin's UI. ``on_resume`` fires
    when the operator switches back. Plugins persist transient state
    via ``ctx.config.set`` during pause.
    """

    def __init__(self, ipc: PluginIpcClient) -> None:
        self._ipc = ipc

    async def on_pause(
        self, handler: Callable[[dict], Awaitable[None] | None]
    ) -> None:
        await self._ipc.event_subscribe(
            f"plugin.{self._ipc._plugin_id}.lifecycle.pause", handler
        )

    async def on_resume(
        self, handler: Callable[[dict], Awaitable[None] | None]
    ) -> None:
        await self._ipc.event_subscribe(
            f"plugin.{self._ipc._plugin_id}.lifecycle.resume", handler
        )


# ---------------------------------------------------------------------
# PluginContext
# ---------------------------------------------------------------------


class PluginContext:
    """The object handed to every lifecycle hook on the plugin class.

    v1.0 shipped a thin shape with identity and ``events`` only;
    reference plugins compensated by hand-injecting internal handles.
    v1.1 fills the SDK: every host-facing surface is a
    capability-gated facade on this class. Plugins program against
    the typed interface; the IPC client is an implementation detail.

    Backward-compat aliases:

    * ``ctx.peripherals`` is an alias for ``ctx.peripheral_manager``.
    * ``_BarePluginContext`` (in :mod:`ados.plugins.ipc_client`) is a
      strict subclass for v1.0 lifecycle hooks that did not connect.
    """

    def __init__(
        self,
        *,
        plugin_id: str,
        plugin_version: str,
        config: dict,
        ipc: PluginIpcClient,
        agent_id: str = "",
        data_dir: Path | None = None,
        config_dir: Path | None = None,
        temp_dir: Path | None = None,
    ) -> None:
        self.plugin_id = plugin_id
        self.plugin_version = plugin_version
        self.config = config
        self.agent_id = agent_id
        self.data_dir = data_dir
        self.config_dir = config_dir
        self.temp_dir = temp_dir
        self.log = get_logger(f"plugin.{plugin_id}")
        self.events = _EventsClient(ipc)
        self.mavlink = _MAVLinkClient(ipc)
        self.peripheral_manager = _PeripheralManagerClient(ipc)
        # Legacy alias kept so v1.0 plugins (e.g., the thermal camera)
        # keep working without changes to their on_start body.
        self.peripherals = self.peripheral_manager
        self.camera = _CameraClient(ipc)
        # Vision engine facade: frame subscription (shared-memory ring),
        # model registration, inference, detection publishing, and
        # visual-odometry pose injection. The host gates the vision caps.
        # Imported lazily so the plugins package does not pull the full SDK
        # graph (which re-imports this module via the test harness) at load.
        from ados.sdk.vision import VisionClient

        self.vision = VisionClient(ipc)
        self.telemetry = _TelemetryClient(ipc)
        self.config_kv = _ConfigClient(ipc, config)
        self.process = _ProcessClient(ipc)
        self.lifecycle = _LifecycleClient(ipc)
        self._ipc = ipc

    async def ping_supervisor(self) -> dict:
        return await self._ipc.ping()


__all__ = [
    "PluginContext",
    "VisionClient",
    "_EventsClient",
    "_MAVLinkClient",
    "_PeripheralManagerClient",
    "_TelemetryClient",
    "_ConfigClient",
    "_ProcessClient",
    "_LifecycleClient",
    "_matches",
]
