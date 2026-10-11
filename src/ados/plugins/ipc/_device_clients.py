"""Device facades on :class:`ados.plugins.ipc.context.PluginContext`: peripheral
drivers, camera claims and frames, and video sources."""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from ados.plugins.ipc_client import PluginIpcClient


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


class _VideoClient:
    """``ctx.video`` facade.

    A driver plugin (e.g. a smart-camera / optical-pod driver) declares the
    camera and stream sources the agent's video pipeline should serve, so the
    operator never hand-types an RTSP URL. The host forwards the source list to
    the supervisor, which persists it and restarts the video pipeline.
    """

    def __init__(self, ipc: PluginIpcClient) -> None:
        self._ipc = ipc

    async def set_source(self, cameras: list[dict]) -> dict:
        """Configure the video pipeline's stream sources.

        ``cameras`` is a list of legs, each ``{"id", "source", "role"?,
        "codec"?}``: ``id`` names the stream path (the primary leg is
        ``main``), ``source`` is an RTSP/URL the pipeline pulls or
        ``publisher`` for a locally-encoded leg, and ``role`` (e.g. ``eo`` /
        ``ir`` / ``eo_wide``) and ``codec`` are advisory. Applying a new list
        restarts the video pipeline. Requires the ``video.source.set``
        capability.

        Returns ``{"ok": bool, "count": int, "restarted": bool}`` (or a
        ``not_available`` shape when the supervisor is unreachable).
        """
        return await self._ipc.video_source_set(cameras)
