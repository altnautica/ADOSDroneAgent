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
import inspect
from collections.abc import Awaitable, Callable
from pathlib import Path
from typing import Any

from ados.core.logging import get_logger
from ados.plugins._dispatch_generated import REQUIRED_CAP
from ados.plugins.errors import CapabilityDenied, PluginError
from ados.plugins.rpc import (
    CapabilityToken,
    Envelope,
    FrameError,
    TokenError,
    encode_frame,
    read_frame,
)

# The wire method the host sends to ask the plugin to run one of its declared
# tools; the runner replies with a correlated response. Gated on the cap the
# generated table names for it, so the two sides cannot drift.
_TOOL_INVOKE_METHOD = "tool.invoke"

# The event method the host pushes a rotated capability token on. A token has a
# 600 s TTL and a plugin runs for a whole flight, so the host re-mints ahead of
# expiry and on every permission change; this is how the plugin's own copy
# stays current. The args are ``{token, expires_at, granted_caps}``.
#
# The plugin MUST adopt it: the token it presents on the next request is what
# the host gates against, and a stale one is refused. Before this existed
# nothing rotated at all, so every gated call from every plugin started
# failing ``token_expired`` ten minutes after the host began serving, with the
# plugin process still up and only per-call errors in its own log.
_TOKEN_REFRESH_METHOD = "token.refresh"

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


log = get_logger("plugins.ipc_client")

DEFAULT_REQUEST_TIMEOUT_S = 5.0

#: Host-pushed deliveries waiting for the plugin's callbacks. Bounded so a
#: callback slower than the stream it subscribed to sheds the newest frames
#: instead of growing the runner without limit.
DELIVERY_QUEUE_MAX = 1024

#: Log one line per this many dropped deliveries, so a saturated callback is
#: visible without one log line per frame.
DELIVERY_DROP_LOG_EVERY = 100


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

    # ---- MSP (Betaflight / iNav / KISS byte plane) --------------------

    async def msp_send(self, msg_bytes: bytes) -> dict:
        """Write already-framed MSP bytes (built with ``ados_protocol::msp`` /
        the plugin's own codec) to the FC. Gated on ``msp.write``."""
        return (
            await self._send_request(
                "msp.send",
                capability="msp.write",
                args={"msg_bytes": bytes(msg_bytes)},
            )
        ).args

    async def msp_subscribe(
        self,
        callback: Callable[[dict], Awaitable[None] | None],
    ) -> None:
        """Subscribe to the raw FC->host MSP byte stream. Deliveries arrive as
        ``msp.deliver`` events carrying ``{bytes, timestamp_ms}``; MSP has no
        per-message topic, so the callback sees every chunk. Gated on
        ``msp.read``."""
        self._msp_callbacks.append(callback)
        await self._send_request(
            "msp.subscribe",
            capability="msp.read",
            args={},
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

    async def vision_read_model(self) -> list[dict]:
        """Read this plugin's resolved model-delivery status.

        Returns one dict per declared model — ``{state, model_id, runtime, path,
        reason}`` (the resolver's ``ModelResolution.to_dict()``), keyed by
        ``model_id`` — so the plugin can find where its delivered model was
        cached (``path`` when ``state == "resolved"``). An unresolved plugin
        returns an empty list rather than raising, so a caller can poll.
        """
        resp = await self._send_request(
            "vision.read_model",
            capability="vision.model.read",
            args={},
        )
        return (resp.args or {}).get("models", [])

    def register_button_callback(
        self,
        callback: Callable[[dict], Awaitable[None] | None],
    ) -> None:
        """Register a callback for ``button.deliver`` events.

        Like the detection stream, the event carries no ``topic`` and is routed
        by method name, so it needs its own callback list rather than riding the
        generic topic dispatch. The callback receives the press directly:
        ``{pin, kind, action, timestamp_ms}``, where ``action`` is ``None`` for an
        unmapped button.
        """
        self._button_callbacks.append(callback)

    async def button_subscribe(self) -> dict:
        """Arm the host's front-panel button push stream for this connection."""
        return await self._send_request(
            "button.subscribe",
            capability="button.subscribe",
            args={},
        )

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

    # ---- Video source -------------------------------------------------

    async def video_source_set(self, cameras: list[dict]) -> dict:
        """Declare the video pipeline's stream sources. The host forwards the
        list to the supervisor, which persists ``video.cameras`` and restarts
        the pipeline. Gated on the ``video.source.set`` capability."""
        return (
            await self._send_request(
                "video.source.set",
                capability="video.source.set",
                args={"cameras": list(cameras)},
            )
        ).args

    # ---- Flight setpoints ---------------------------------------------

    async def flight_guided_setpoint_send(self, setpoint: dict) -> dict:
        """Send a guided-mode position/velocity setpoint to the flight
        controller. The host forwards it to the MAVLink router, which encodes
        the appropriate SET_POSITION_TARGET message. Gated on the
        ``flight.guided_setpoint`` capability."""
        return (
            await self._send_request(
                "flight.guided_setpoint.send",
                capability="flight.guided_setpoint",
                args=dict(setpoint),
            )
        ).args

    async def flight_rate_setpoint_send(self, setpoint: dict) -> dict:
        """Send an attitude/body-rate + thrust setpoint (SET_ATTITUDE_TARGET) to
        the flight controller. The host forwards it to the MAVLink router, which
        encodes the SET_ATTITUDE_TARGET message. Gated on the
        ``flight.rate_setpoint`` capability."""
        return (
            await self._send_request(
                "flight.rate_setpoint.send",
                capability="flight.rate_setpoint",
                args=dict(setpoint),
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

    def _adopt_refreshed_token(self, env: Envelope) -> None:
        """Replace the token this client presents with the host's fresh one.

        Called for a ``token.refresh`` event. The host re-mints from
        authoritative state, so the new token reflects the operator's CURRENT
        grant set — a capability that was just revoked is absent from it, and
        the plugin's next call using that capability is correctly denied. The
        granted set is stored too so a ``tool.invoke`` gate and any plugin-side
        capability introspection stay consistent with the wire.

        A malformed payload is ignored rather than clearing the token: keeping
        a working token beats dropping to one that cannot authenticate over a
        bad frame.
        """
        token = (env.args or {}).get("token")
        if not isinstance(token, str) or not token:
            log.warning(
                "plugin_token_refresh_malformed", plugin_id=self._plugin_id
            )
            return
        try:
            parsed = CapabilityToken.from_string(token)
        except TokenError as exc:
            log.warning(
                "plugin_token_refresh_unparseable",
                plugin_id=self._plugin_id,
                error=str(exc),
            )
            return
        lost = self._granted_caps - parsed.granted_caps
        self._token = token
        self._granted_caps = parsed.granted_caps
        log.info(
            "plugin_token_refreshed",
            plugin_id=self._plugin_id,
            expires_at=parsed.expires_at,
            # Named explicitly: a plugin losing a capability mid-flight is
            # something its author needs to see in the log, not something to
            # discover through a later capability_denied.
            revoked=sorted(lost),
        )

    async def _handle_tool_invoke(self, env: Envelope) -> None:
        """Run a declared tool for a host ``tool.invoke`` request and reply.

        Gated on the cap the generated table names for ``tool.invoke``
        (``mcp.expose``): a plugin whose token lacks it never runs a tool. An
        unknown tool or a handler exception is surfaced as an error response,
        never a silent drop, so the host's pending future always resolves.
        """
        required = REQUIRED_CAP.get(_TOOL_INVOKE_METHOD)
        if required is not None and required not in self._granted_caps:
            await self._reply(env.request_id, args={}, error=f"capability_denied: {required}")
            return
        tool = env.args.get("tool")
        if not isinstance(tool, str):
            await self._reply(env.request_id, args={}, error="tool.invoke: missing tool name")
            return
        handler = self._tool_handlers.get(tool)
        if handler is None:
            await self._reply(env.request_id, args={}, error=f"tool_not_found: {tool}")
            return
        raw_args = env.args.get("arguments")
        arguments = raw_args if isinstance(raw_args, dict) else {}
        try:
            result = handler(arguments)
            if inspect.isawaitable(result):
                result = await result
        except Exception as exc:  # noqa: BLE001
            await self._reply(env.request_id, args={}, error=f"tool_error: {exc}")
            return
        payload = result if isinstance(result, dict) else {"result": result}
        await self._reply(env.request_id, args=payload, error=None)

    async def _reply(
        self, request_id: str, *, args: dict, error: str | None
    ) -> None:
        """Write a response envelope back to the host for a tool.invoke."""
        if self._writer is None:
            return
        env = Envelope(
            type="response",
            method="response",
            capability="",
            args=args,
            request_id=request_id,
            token="",
            error=error,
        )
        try:
            self._writer.write(encode_frame(env))
            await self._writer.drain()
        except (ConnectionError, BrokenPipeError):
            pass

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

    async def _dispatch_button(self, env: Envelope) -> None:
        # The press rides as a decoded map, not a byte blob: it is four small
        # scalars, so the callback gets it directly with no second decode step.
        press = env.args.get("press") or {}
        for cb in list(self._button_callbacks):
            try:
                result = cb(press)
                if asyncio.iscoroutine(result):
                    await result
            except Exception:  # noqa: BLE001 - one bad callback must not stop the stream
                log.exception("button_callback_failed")

    async def _dispatch_msp(self, env: Envelope) -> None:
        # The whole args map ({bytes, timestamp_ms}) goes to the callback; the
        # plugin's codec parses the raw MSP bytes.
        for cb in list(self._msp_callbacks):
            try:
                result = cb(env.args)
                if asyncio.iscoroutine(result):
                    await result
            except Exception:  # noqa: BLE001 - one bad callback must not stop the stream
                log.exception("msp_callback_failed")

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
