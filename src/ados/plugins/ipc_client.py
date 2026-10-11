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
* ``ctx.telemetry.extend`` — extend the heartbeat schema;
  ``subscribe_telemetry`` — the live vehicle-state stream.
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

from ados.plugins._ipc_client_api import _PluginApiMixin
from ados.plugins._ipc_client_common import (
    _TOKEN_REFRESH_METHOD,
    _TOOL_INVOKE_METHOD,
    DEFAULT_REQUEST_TIMEOUT_S,
    DELIVERY_DROP_LOG_EVERY,
    DELIVERY_QUEUE_MAX,
    AllowlistViolation,
    InvalidComponent,
    log,
)
from ados.plugins._ipc_client_common import (
    TELEMETRY_STATE_TOPIC as TELEMETRY_STATE_TOPIC,
)
from ados.plugins._ipc_client_common import (
    _driver_kind_capability as _driver_kind_capability,
)
from ados.plugins._ipc_client_dispatch import _DeliveryMixin
from ados.plugins.errors import CapabilityDenied, PluginError
from ados.plugins.rpc import (
    CapabilityToken,
    Envelope,
    FrameError,
    TokenError,
    encode_frame,
    read_frame,
)


class PluginIpcClient(_PluginApiMixin, _DeliveryMixin):
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
        self._button_callbacks: list[
            Callable[[dict], Awaitable[None] | None]
        ] = []
        # MSP deliveries arrive as ``msp.deliver`` events carrying no ``topic``
        # (MSP has no per-message topic), routed by method name to these
        # callbacks. Each carries ``{bytes, timestamp_ms}``.
        self._msp_callbacks: list[
            Callable[[dict], Awaitable[None] | None]
        ] = []
        self._reader_task: asyncio.Task | None = None
        self._dispatch_task: asyncio.Task | None = None
        # Deliveries (events, MAVLink, detections, buttons, MSP) are handed from
        # the reader to a dispatcher task instead of being awaited inline: a
        # callback that issues a request would otherwise wait for a response
        # frame that only the blocked reader could read.
        self._deliveries: asyncio.Queue[Envelope] = asyncio.Queue(
            maxsize=DELIVERY_QUEUE_MAX
        )
        self._dropped_deliveries = 0
        # tool.invoke handlers run as their own tasks for the same reason; the
        # set keeps a reference so a running handler is not garbage collected.
        self._tool_tasks: set[asyncio.Task] = set()
        # Set once the connection to the host is gone (EOF, frame error, or
        # close). The runner exits on it so systemd restarts the plugin and the
        # bridge is re-established, instead of a plugin that stays up and inert.
        self.disconnected = asyncio.Event()
        self._next_id = 0
        # MCP tool handlers the plugin registers (the SDK @tool decorator fills
        # this). The host asks the plugin to run one by name via a tool.invoke
        # request; the runner looks it up here and replies with the result.
        self._tool_handlers: dict[
            str, Callable[[dict], Awaitable[Any] | Any]
        ] = {}
        # The plugin's own granted caps, parsed once from its token (parse only,
        # no HMAC verify — the runner trusts its own token; the host verified it).
        # An unparseable token yields an empty set so a tool.invoke fails closed.
        try:
            self._granted_caps: frozenset[str] = CapabilityToken.from_string(
                token
            ).granted_caps
        except TokenError:
            self._granted_caps = frozenset()

    def register_tool(
        self, name: str, handler: Callable[[dict], Awaitable[Any] | Any]
    ) -> None:
        """Register an MCP tool handler by name. The SDK ``@tool`` decorator
        calls this from the plugin's declared tool set; the host routes a
        ``tool.invoke`` for ``name`` to ``handler(arguments)``."""
        self._tool_handlers[name] = handler

    async def connect(self) -> None:
        self._reader, self._writer = await asyncio.open_unix_connection(
            str(self._socket_path)
        )
        self._reader_task = asyncio.create_task(self._reader_loop())
        self._dispatch_task = asyncio.create_task(self._dispatch_loop())
        # Handshake. A refused or dropped hello leaves nothing running: the
        # runner retries with a fresh client, so this one must not linger.
        try:
            await self._send_request("hello", capability="", args={})
        except BaseException:
            await self.close()
            raise
        log.info("plugin_ipc_client_connected", plugin_id=self._plugin_id)

    async def close(self) -> None:
        # Set first: a deliberate close is not a lost connection.
        self.disconnected.set()
        for task in (self._reader_task, self._dispatch_task, *self._tool_tasks):
            if task is not None:
                task.cancel()
        if self._writer is not None:
            try:
                self._writer.close()
                await self._writer.wait_closed()
            except (ConnectionError, RuntimeError):
                pass

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
        if self._writer is None or self.disconnected.is_set():
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
        """Route every frame from the host; never run plugin code here.

        Responses resolve their pending future and token refreshes are adopted
        inline (both are quick and must stay ordered with the frames around
        them). Deliveries go to the dispatcher queue and a ``tool.invoke`` runs
        as its own task, so a callback or tool handler that issues a request can
        always receive its response.
        """
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
                    if env.method == _TOKEN_REFRESH_METHOD:
                        self._adopt_refreshed_token(env)
                    else:
                        self._enqueue_delivery(env)
                elif env.type == "request" and env.method == _TOOL_INVOKE_METHOD:
                    task = asyncio.create_task(self._handle_tool_invoke(env))
                    self._tool_tasks.add(task)
                    task.add_done_callback(self._tool_tasks.discard)
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
        finally:
            self._on_connection_lost()

    def _on_connection_lost(self) -> None:
        """Mark the bridge gone and fail every in-flight request at once, so no
        caller sits out its full timeout on a connection that cannot answer."""
        if not self.disconnected.is_set():
            log.warning("plugin_ipc_disconnected", plugin_id=self._plugin_id)
        self.disconnected.set()
        for fut in self._pending.values():
            if not fut.done():
                fut.set_exception(PluginError("ipc connection to the host closed"))

    def _enqueue_delivery(self, env: Envelope) -> None:
        try:
            self._deliveries.put_nowait(env)
        except asyncio.QueueFull:
            self._dropped_deliveries += 1
            if self._dropped_deliveries % DELIVERY_DROP_LOG_EVERY == 1:
                log.warning(
                    "plugin_ipc_deliveries_dropped",
                    plugin_id=self._plugin_id,
                    dropped_total=self._dropped_deliveries,
                    method=env.method,
                )

    async def _dispatch_loop(self) -> None:
        """Run the plugin's callbacks for each delivery, in arrival order."""
        while True:
            env = await self._deliveries.get()
            try:
                if env.method == "mavlink.deliver":
                    await self._dispatch_mavlink(env)
                elif env.method == "vision.deliver_detection":
                    await self._dispatch_detection(env)
                elif env.method == "button.deliver":
                    await self._dispatch_button(env)
                elif env.method == "msp.deliver":
                    await self._dispatch_msp(env)
                else:
                    await self._dispatch_event(env)
            except asyncio.CancelledError:
                raise
            except Exception as exc:  # noqa: BLE001 - one bad delivery must not stop the stream
                log.error(
                    "plugin_ipc_dispatch_failed",
                    plugin_id=self._plugin_id,
                    method=env.method,
                    error=str(exc),
                )


# PluginContext lives in :mod:`ados.plugins.ipc.context` so this module stays
# focused on the wire-level client; it is re-exported here because the runner
# and the SDK test harness build it next to the client.
from ados.plugins.ipc.context import PluginContext  # noqa: E402

__all__ = [
    "AllowlistViolation",
    "InvalidComponent",
    "PluginContext",
    "PluginIpcClient",
]
