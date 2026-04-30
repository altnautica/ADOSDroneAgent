"""Plugin runner side of the IPC bridge.

The :class:`PluginContext` exposed to plugin code wraps a
:class:`PluginIpcClient` so plugin authors call simple methods
(``ctx.events.publish(...)``, ``ctx.events.subscribe(...)``) and the
client serializes them as RPC envelopes to the supervisor's UDS.

Current public surface:

* ``ctx.events.publish(topic, payload)`` — capability-gated.
* ``ctx.events.subscribe(topic_pattern, callback)`` — async iterator.
* ``ctx.ping()`` — health probe.

MAVLink, HAL, telemetry-extension wrappers land as additional methods
on the supervisor's IPC server.
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


def _matches(pattern: str, topic: str) -> bool:
    import fnmatch as _fnm

    return _fnm.fnmatchcase(topic, pattern)


# ---------------------------------------------------------------------------
# PluginContext: the public API plugin code sees
# ---------------------------------------------------------------------------


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


class PluginContext:
    """The object handed to every lifecycle hook on the plugin class.

    Currently exposes ``events`` + ``log`` + identity. ``mavlink``,
    ``hal``, ``telemetry``, ``recording``, ``peripherals``,
    ``network`` are added as additional sub-clients when the
    supervisor wires the corresponding host handlers. Plugin code
    should program against the typed interface on this class, not
    the underlying IPC client.
    """

    def __init__(
        self,
        *,
        plugin_id: str,
        plugin_version: str,
        config: dict,
        ipc: PluginIpcClient,
    ) -> None:
        self.plugin_id = plugin_id
        self.plugin_version = plugin_version
        self.config = config
        self.events = _EventsClient(ipc)
        self.log = get_logger(f"plugin.{plugin_id}")
        self._ipc = ipc

    async def ping_supervisor(self) -> dict:
        return await self._ipc.ping()
