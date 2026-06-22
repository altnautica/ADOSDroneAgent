"""Plugin runner side of the IPC bridge.

The :class:`PluginContext` exposed to plugin code wraps a
:class:`PluginIpcClient` so plugin authors call simple methods
(``ctx.events.publish(...)``, ``ctx.mavlink.send(...)``,
``ctx.peripheral_manager.register_camera_driver(...)`` ) and the
client serializes them as RPC envelopes to the supervisor's UDS.

Public surface today:

* ``ctx.events.publish / subscribe`` — event bus.
* ``ctx.mavlink.send / subscribe / register_component`` — MAVLink
  read and write through the host's router.
* ``ctx.peripheral_manager.register_*_driver`` and ``camera.claim`` —
  driver registration plus exclusive camera holds.
* ``ctx.telemetry.extend`` — extend the heartbeat schema.
* ``ctx.config.get / set`` — per-drone or global kv.
* ``ctx.process.spawn`` — sandboxed vendor-binary spawn with
  allowlist enforcement on the supervisor side.
* ``ctx.lifecycle.on_pause / on_resume`` — GCS-side mount events.
* ``ctx.ping_supervisor()`` — health probe.
"""

from __future__ import annotations

import asyncio
from collections.abc import Awaitable, Callable
from pathlib import Path
from typing import Any

from ados.core.logging import get_logger
from ados.plugins.errors import CapabilityDenied, PluginError
from ados.plugins.rpc import (
    Envelope,
    FrameError,
    encode_frame,
    read_frame,
)

# ---------------------------------------------------------------------
# Typed exceptions surfaced to plugin code
# ---------------------------------------------------------------------


class InvalidComponent(PluginError):
    """Raised when a plugin sends to a component_id it has not reserved."""


class AllowlistViolation(PluginError):
    """Raised when ``ctx.process.spawn`` rejects a basename that is not on
    the manifest's ``agent.subprocess_spawn`` allowlist."""

    def __init__(self, basename: str) -> None:
        super().__init__(
            f"binary basename {basename!r} not on subprocess_spawn allowlist"
        )
        self.basename = basename


class HostUnavailable(PluginError):
    """Raised when the host service the call routes through has not
    been wired yet (e.g., the FC connection has not initialized)."""

log = get_logger("plugins.ipc_client")

DEFAULT_REQUEST_TIMEOUT_S = 5.0


class PluginIpcClient:
    """Async client. One instance per plugin runner process."""

    def __init__(self, *, plugin_id: str, token: str, socket_path: Path) -> None:
        self._plugin_id = plugin_id
        self._token = token
        self._socket_path = socket_path
        self._reader: asyncio.StreamReader | None = None
        self._writer: asyncio.StreamWriter | None = None
        self._pending: dict[str, asyncio.Future[Envelope]] = {}
        self._event_callbacks: dict[
            str, list[Callable[[dict], Awaitable[None] | None]]
        ] = {}
        self._mavlink_callbacks: dict[
            str, list[Callable[[dict], Awaitable[None] | None]]
        ] = {}
        # Vision detection deliveries arrive as ``vision.deliver_detection``
        # events carrying no ``topic`` field, so they are routed by method name
        # (like ``mavlink.deliver``) to these callbacks rather than through the
        # topic-based event surface.
        self._detection_callbacks: list[
            Callable[[dict], Awaitable[None] | None]
        ] = []
        self._reader_task: asyncio.Task | None = None
        self._next_id = 0

    async def connect(self) -> None:
        self._reader, self._writer = await asyncio.open_unix_connection(
            str(self._socket_path)
        )
        self._reader_task = asyncio.create_task(self._reader_loop())
        # Handshake.
        await self._send_request("hello", capability="", args={})
        log.info("plugin_ipc_client_connected", plugin_id=self._plugin_id)

    async def close(self) -> None:
        if self._reader_task is not None:
            self._reader_task.cancel()
        if self._writer is not None:
            try:
                self._writer.close()
                await self._writer.wait_closed()
            except (ConnectionError, RuntimeError):
                pass

    async def ping(self) -> dict:
        return (await self._send_request("ping", capability="", args={})).args

    async def event_publish(self, topic: str, payload: dict) -> int:
        result = await self._send_request(
            "event.publish",
            capability="event.publish",
            args={"topic": topic, "payload": payload},
        )
        return int(result.args.get("delivered", 0))

    async def event_subscribe(
        self,
        topic_pattern: str,
        callback: Callable[[dict], Awaitable[None] | None],
    ) -> None:
        self._event_callbacks.setdefault(topic_pattern, []).append(callback)
        await self._send_request(
            "event.subscribe",
            capability="event.subscribe",
            args={"topic": topic_pattern},
        )

    # ---- MAVLink ------------------------------------------------------

    async def mavlink_send(
        self, msg_bytes: bytes, component_id: int | None = None
    ) -> dict:
        args: dict[str, Any] = {"msg_bytes": bytes(msg_bytes)}
        if component_id is not None:
            args["component_id"] = int(component_id)
        return (
            await self._send_request(
                "mavlink.send", capability="mavlink.write", args=args
            )
        ).args

    async def mavlink_register_component(self, comp_id: int, kind: str) -> dict:
        return (
            await self._send_request(
                "mavlink.register_component",
                capability=f"mavlink.component.{kind}",
                args={"component_id": int(comp_id), "kind": kind},
            )
        ).args

    async def mavlink_subscribe(
        self,
        msg_name: str,
        callback: Callable[[dict], Awaitable[None] | None],
    ) -> None:
        # MAVLink deliveries arrive as ``event``-type envelopes routed
        # through the same reader loop as event bus deliveries; the
        # dispatcher routes them by topic.
        self._mavlink_callbacks.setdefault(msg_name, []).append(callback)
        await self._send_request(
            "mavlink.subscribe",
            capability="mavlink.read",
            args={"msg_name": msg_name},
        )

    # ---- Vision -------------------------------------------------------

    async def vision_subscribe_detections(
        self,
        callback: Callable[[dict], Awaitable[None] | None],
    ) -> None:
        """Register a callback for ``vision.deliver_detection`` events.

        The subscribe RPC is sent by the SDK facade; this only records the local
        deliver callback. Each delivered batch is routed here by method name (the
        event carries no ``topic``) and the callback receives ``{batch,
        timestamp_ms}``.
        """
        self._detection_callbacks.append(callback)

    # ---- Telemetry ----------------------------------------------------

    async def telemetry_extend(self, channel: str, payload: dict) -> dict:
        return (
            await self._send_request(
                "telemetry.extend",
                capability="telemetry.extend",
                args={"channel": channel, "payload": payload},
            )
        ).args

    # ---- Peripheral manager ------------------------------------------

    async def peripheral_register_driver(self, kind: str, driver_ref: str) -> dict:
        return (
            await self._send_request(
                "peripheral.register_driver",
                capability=f"sensor.{kind}.register",
                args={"kind": kind, "driver_ref": driver_ref},
            )
        ).args

    async def peripheral_unregister_driver(self, handle_id: str) -> dict:
        return (
            await self._send_request(
                "peripheral.unregister_driver",
                capability="",
                args={"handle_id": handle_id},
            )
        ).args

    async def camera_claim(self, device_path: str, exclusive: bool = True) -> dict:
        return (
            await self._send_request(
                "camera.claim",
                capability="sensor.camera.register",
                args={"device_path": device_path, "exclusive": exclusive},
            )
        ).args

    async def camera_release(self, device_path: str) -> dict:
        return (
            await self._send_request(
                "camera.release",
                capability="sensor.camera.register",
                args={"device_path": device_path},
            )
        ).args

    async def camera_get_frame(
        self,
        device_path: str,
        *,
        format: str = "nv12",
        timeout_ms: int = 1000,
    ) -> dict:
        return (
            await self._send_request(
                "camera.get_frame",
                capability="sensor.camera.register",
                args={
                    "device_path": device_path,
                    "format": format,
                    "timeout_ms": int(timeout_ms),
                },
            )
        ).args

    # ---- Config kv ----------------------------------------------------

    async def config_get(self, key: str, default: Any = None) -> Any:
        resp = await self._send_request(
            "config.get",
            capability="",
            args={"key": key, "default": default},
        )
        return resp.args.get("value")

    async def config_set(
        self, key: str, value: Any, scope: str = "drone"
    ) -> dict:
        return (
            await self._send_request(
                "config.set",
                capability="",
                args={"key": key, "value": value, "scope": scope},
            )
        ).args

    # ---- Process spawn (sandboxed) -----------------------------------

    async def process_spawn(
        self,
        basename: str,
        args: list[str] | None = None,
        env: dict[str, str] | None = None,
    ) -> dict:
        """Authorize a vendor-binary spawn through the supervisor.

        The supervisor enforces the manifest allowlist and audit-logs
        the attempt. On success it returns the resolved install dir
        plus the original args/env; the actual exec happens in the
        plugin runner process via
        :func:`ados.plugins.process_sandbox.spawn` so the child
        inherits the runner's cgroup slice.
        """
        return (
            await self._send_request(
                "process.spawn",
                capability="process.spawn",
                args={
                    "basename": basename,
                    "args": list(args or []),
                    "env": dict(env or {}),
                },
            )
        ).args

    # ------------------------------------------------------------------
    # Internals
    # ------------------------------------------------------------------

    async def _send_request(
        self,
        method: str,
        *,
        capability: str,
        args: dict,
        timeout_s: float = DEFAULT_REQUEST_TIMEOUT_S,
    ) -> Envelope:
        if self._writer is None:
            raise PluginError("ipc client not connected")
        self._next_id += 1
        rid = f"r{self._next_id}"
        env = Envelope(
            type="request",
            method=method,
            capability=capability,
            args=args,
            request_id=rid,
            token=self._token,
        )
        future: asyncio.Future[Envelope] = asyncio.get_event_loop().create_future()
        self._pending[rid] = future
        try:
            self._writer.write(encode_frame(env))
            await self._writer.drain()
        except (ConnectionError, BrokenPipeError) as exc:
            self._pending.pop(rid, None)
            raise PluginError(f"ipc write failed: {exc}") from exc
        try:
            response = await asyncio.wait_for(future, timeout=timeout_s)
        finally:
            self._pending.pop(rid, None)
        if response.error:
            if "not permitted" in response.error:
                raise CapabilityDenied(self._plugin_id, capability)
            if response.error.startswith("capability_denied:"):
                # Format: "capability_denied: <cap>". The dispatcher
                # gate rejects callers whose token does not carry the
                # capability the method declares.
                cap = response.error.split(":", 1)[1].strip()
                raise CapabilityDenied(self._plugin_id, cap)
            if response.error.startswith("allowlist_violation:"):
                basename = response.error.split(":", 1)[1].strip()
                raise AllowlistViolation(basename)
            raise PluginError(f"rpc error: {response.error}")
        return response

    async def _reader_loop(self) -> None:
        assert self._reader is not None
        try:
            while True:
                try:
                    env = await read_frame(self._reader)
                except FrameError as exc:
                    log.warning(
                        "plugin_ipc_frame_error",
                        plugin_id=self._plugin_id,
                        error=str(exc),
                    )
                    return
                if env is None:
                    return
                if env.type == "event":
                    if env.method == "mavlink.deliver":
                        await self._dispatch_mavlink(env)
                    elif env.method == "vision.deliver_detection":
                        await self._dispatch_detection(env)
                    else:
                        await self._dispatch_event(env)
                else:
                    fut = self._pending.get(env.request_id)
                    if fut is not None and not fut.done():
                        fut.set_result(env)
        except asyncio.CancelledError:
            return
        except Exception as exc:  # noqa: BLE001
            log.error(
                "plugin_ipc_reader_loop_unhandled",
                plugin_id=self._plugin_id,
                error=str(exc),
            )

    async def _dispatch_event(self, env: Envelope) -> None:
        topic: Any = env.args.get("topic")
        payload: Any = env.args.get("payload")
        if not isinstance(topic, str):
            return
        for pattern, callbacks in list(self._event_callbacks.items()):
            if pattern == topic or _matches(pattern, topic):
                for cb in callbacks:
                    try:
                        result = cb(payload if isinstance(payload, dict) else {})
                        if asyncio.iscoroutine(result):
                            await result
                    except Exception as exc:  # noqa: BLE001
                        log.error(
                            "plugin_event_callback_error",
                            plugin_id=self._plugin_id,
                            topic=topic,
                            error=str(exc),
                        )

    async def _dispatch_detection(self, env: Envelope) -> None:
        payload = {
            "batch": env.args.get("batch"),
            "timestamp_ms": env.args.get("timestamp_ms"),
        }
        for cb in list(self._detection_callbacks):
            try:
                result = cb(payload)
                if asyncio.iscoroutine(result):
                    await result
            except Exception as exc:  # noqa: BLE001
                log.error(
                    "plugin_detection_callback_error",
                    plugin_id=self._plugin_id,
                    error=str(exc),
                )

    async def _dispatch_mavlink(self, env: Envelope) -> None:
        msg_name: Any = env.args.get("msg_name")
        if not isinstance(msg_name, str):
            return
        frame: Any = env.args.get("frame")
        payload = {
            "msg_name": msg_name,
            "frame": frame if isinstance(frame, (bytes, bytearray)) else bytes(frame or b""),
            "timestamp_ms": env.args.get("timestamp_ms"),
        }
        for pattern, callbacks in list(self._mavlink_callbacks.items()):
            if pattern == msg_name or _matches(pattern, msg_name):
                for cb in callbacks:
                    try:
                        result = cb(payload)
                        if asyncio.iscoroutine(result):
                            await result
                    except Exception as exc:  # noqa: BLE001
                        log.error(
                            "plugin_mavlink_callback_error",
                            plugin_id=self._plugin_id,
                            msg_name=msg_name,
                            error=str(exc),
                        )


def _matches(pattern: str, topic: str) -> bool:
    import fnmatch as _fnm

    return _fnm.fnmatchcase(topic, pattern)


# ---------------------------------------------------------------------------
# PluginContext: re-export the public surface from the context module.
# The facade classes live in :mod:`ados.plugins.ipc.context` so this module
# stays focused on the wire-level IPC client. Existing imports of
# :class:`PluginContext` from :mod:`ados.plugins.ipc_client` still work.
# ---------------------------------------------------------------------------


from ados.plugins.ipc.context import (  # noqa: E402, F401
    PluginContext,
    _ConfigClient,
    _EventsClient,
    _LifecycleClient,
    _MAVLinkClient,
    _PeripheralManagerClient,
    _ProcessClient,
    _TelemetryClient,
)


class _NullIpcClient:
    """No-op stand-in for :class:`PluginIpcClient`.

    Used by :class:`_BarePluginContext` when no real IPC bridge has
    been wired yet (early supervisor boot, certain test paths). Every
    method raises :class:`HostUnavailable`. The shape mirrors the
    real client so attribute lookup succeeds; calls fail loudly.
    """

    def __init__(self, plugin_id: str) -> None:
        self._plugin_id = plugin_id

    def _unavail(self, _name: str) -> Any:
        raise HostUnavailable(
            f"plugin {self._plugin_id}: IPC bridge not connected"
        )

    async def ping(self) -> dict: return self._unavail("ping")
    async def event_publish(self, *_a, **_k) -> int: return self._unavail("event_publish")
    async def event_subscribe(self, *_a, **_k) -> None: return self._unavail("event_subscribe")
    async def mavlink_send(self, *_a, **_k) -> dict: return self._unavail("mavlink_send")
    async def mavlink_subscribe(self, *_a, **_k) -> None: return self._unavail("mavlink_subscribe")
    async def vision_subscribe_detections(self, *_a, **_k) -> None: return self._unavail("vision_subscribe_detections")
    async def mavlink_register_component(self, *_a, **_k) -> dict: return self._unavail("mavlink_register_component")
    async def telemetry_extend(self, *_a, **_k) -> dict: return self._unavail("telemetry_extend")
    async def peripheral_register_driver(self, *_a, **_k) -> dict: return self._unavail("peripheral_register_driver")
    async def peripheral_unregister_driver(self, *_a, **_k) -> dict: return self._unavail("peripheral_unregister_driver")
    async def camera_claim(self, *_a, **_k) -> dict: return self._unavail("camera_claim")
    async def camera_release(self, *_a, **_k) -> dict: return self._unavail("camera_release")
    async def camera_get_frame(self, *_a, **_k) -> dict: return self._unavail("camera_get_frame")
    async def config_get(self, *_a, **_k) -> Any: return self._unavail("config_get")
    async def config_set(self, *_a, **_k) -> dict: return self._unavail("config_set")
    async def process_spawn(self, *_a, **_k) -> dict: return self._unavail("process_spawn")


class _BarePluginContext(PluginContext):
    """Deprecated alias retained for v1.0 lifecycle-hook tests.

    Subclasses :class:`PluginContext` so any call site that type-checks
    against the bare shape still works. New code should construct
    :class:`PluginContext` directly.
    """

    def __init__(
        self,
        *,
        plugin_id: str,
        version: str,
        ipc: PluginIpcClient | None = None,
    ) -> None:
        if ipc is None:
            ipc = _NullIpcClient(plugin_id)  # type: ignore[assignment]
        super().__init__(
            plugin_id=plugin_id,
            plugin_version=version,
            config={},
            ipc=ipc,  # type: ignore[arg-type]
            agent_id="",
        )
