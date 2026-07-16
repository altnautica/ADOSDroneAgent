"""Supervisor-side IPC server.

Binds a Unix domain socket per plugin at ``/run/ados/plugins/<id>.sock``.
The plugin runner connects, presents its capability token, and then
issues request envelopes (event publish/subscribe, MAVLink read/write,
HAL access, etc). Each request is validated against the token's
declared capability set before routing to the host service.

Surface covered today:

* Event bus publish / subscribe (capability-gated)
* MAVLink send / subscribe / component-id reservation
* Telemetry channel extension
* Driver registration (camera / lidar / gimbal / gps / esc /
  payload-actuator) plus camera-path claim
* Config kv with per-drone and global scope
* Sandboxed vendor binary spawn (allowlist enforced via the manifest)

The dispatch loop holds a verified :class:`CapabilityToken` per
session and gates each method on a required capability declared in
the dispatch table. Handler bodies live in
:mod:`ados.plugins.ipc.handlers` so this file stays focused on
transport, handshake, and routing.
"""

from __future__ import annotations

import asyncio
import os
from collections.abc import Awaitable, Callable
from dataclasses import dataclass, field
from pathlib import Path

from ados.core.asyncio_util import log_task_exceptions
from ados.core.logging import get_logger
from ados.core.runtime_mode import is_service_native
from ados.plugins._dispatch_generated import REQUIRED_CAP
from ados.plugins.errors import CapabilityDenied as _CapabilityDenied
from ados.plugins.events import (
    Event,
    EventBus,
    is_publish_allowed,
    is_subscribe_allowed,
    now_ms,
)
from ados.plugins.ipc.host_services import HostServices, default_host_services
from ados.plugins.process_sandbox import AllowlistViolation as _AllowlistViolation
from ados.plugins.rpc import (
    CapabilityToken,
    Envelope,
    FrameError,
    TokenError,
    TokenIssuer,
    encode_frame,
    read_frame,
)

log = get_logger("plugins.ipc_server")

SOCKET_DIR = Path("/run/ados/plugins")


def _native_host_is_active() -> bool:
    """True when the native plugin host owns the per-plugin sockets.

    The native host and this packaged server are mutually exclusive: both
    bind ``/run/ados/plugins/<id>.sock``. When the native binary is present
    and the operator has not pinned the packaged fallback marker, the native
    host is the active owner and this server must not bind. Cheap (it only
    stats files) and total (it never raises), so it is safe to call on the
    socket-bring-up path.
    """
    try:
        return is_service_native("plugin-host")
    except Exception:  # noqa: BLE001
        return False


@dataclass
class PluginSession:
    plugin_id: str
    token: CapabilityToken
    writer: asyncio.StreamWriter
    subscriptions: set[str]
    mavlink_subscriptions: set[str] = field(default_factory=set)
    pump_tasks: list[asyncio.Task] = field(default_factory=list)
    # Host -> plugin request/response: a tool.invoke the host sent THIS plugin,
    # awaiting its correlated reply. Per-session (not server-wide) so a plugin
    # can only resolve its OWN pending invokes — a response frame from one
    # session can never resolve another session's waiter.
    pending_invokes: dict[str, asyncio.Future[Envelope]] = field(
        default_factory=dict
    )


class PluginIpcServer:
    """One server, many connections, one per running plugin.

    Started by the supervisor; each plugin runner connects to its
    dedicated socket. The server is async and runs on the supervisor's
    event loop.
    """

    def __init__(
        self,
        *,
        bus: EventBus,
        token_issuer: TokenIssuer,
        socket_dir: Path = SOCKET_DIR,
        host: HostServices | None = None,
        native_owner_check: Callable[[], bool] | None = None,
    ) -> None:
        self._bus = bus
        self._token_issuer = token_issuer
        self._socket_dir = socket_dir
        self._host = host if host is not None else default_host_services()
        self._servers: dict[str, asyncio.AbstractServer] = {}
        self._sessions: dict[str, PluginSession] = {}
        # Monotonic counter for host->plugin invoke request ids (globally unique,
        # though each pending future lives on its own session's map).
        self._invoke_seq = 0
        # When the native plugin host is the active socket owner this server
        # yields instead of binding (they would otherwise contend for the same
        # <id>.sock). Injectable for tests; defaults to the on-disk cutover
        # state.
        self._native_owner_check = (
            native_owner_check
            if native_owner_check is not None
            else _native_host_is_active
        )

    @property
    def host(self) -> HostServices:
        """Bundle of host-side service facades the handlers route through."""
        return self._host

    @property
    def bus(self) -> EventBus:
        return self._bus

    async def start_for_plugin(self, plugin_id: str) -> Path:
        """Start a UDS server for one plugin. Returns the socket path.

        When the native plugin host is the active socket owner this packaged
        server YIELDS: it returns the socket path without binding, so the two
        implementations never contend for the same ``<id>.sock``. The packaged
        server binds only when the native host is pinned off (the fallback
        marker) or its binary is absent.
        """
        sock_path = self._socket_dir / f"{plugin_id}.sock"
        if self._native_owner_check():
            # The native host owns this socket; do not bind (and do not touch
            # any existing socket file — it may be the native host's live one).
            log.info(
                "plugin_ipc_server_yielding_to_native",
                plugin_id=plugin_id,
                socket=str(sock_path),
            )
            return sock_path

        self._socket_dir.mkdir(parents=True, exist_ok=True)
        # Replace any stale socket from a previous run.
        if sock_path.exists():
            sock_path.unlink()

        server = await asyncio.start_unix_server(
            client_connected_cb=lambda r, w: self._on_connect(plugin_id, r, w),
            path=str(sock_path),
        )
        os.chmod(sock_path, 0o660)
        self._servers[plugin_id] = server
        log.info(
            "plugin_ipc_server_started",
            plugin_id=plugin_id,
            socket=str(sock_path),
        )
        return sock_path

    async def stop_for_plugin(self, plugin_id: str) -> None:
        server = self._servers.pop(plugin_id, None)
        if server is None:
            # We never bound this socket (the native host owns it, or it was
            # never started). Do not unlink — the socket file on disk may be
            # the native host's live one.
            self._sessions.pop(plugin_id, None)
            return
        server.close()
        await server.wait_closed()
        sock_path = self._socket_dir / f"{plugin_id}.sock"
        if sock_path.exists():
            sock_path.unlink()
        self._sessions.pop(plugin_id, None)

    async def _on_connect(
        self,
        plugin_id: str,
        reader: asyncio.StreamReader,
        writer: asyncio.StreamWriter,
    ) -> None:
        peer = writer.get_extra_info("peername")
        log.info("plugin_ipc_client_connected", plugin_id=plugin_id, peer=peer)
        session: PluginSession | None = None
        try:
            session = await self._handshake(plugin_id, reader, writer)
            if session is None:
                writer.close()
                await writer.wait_closed()
                return
            self._sessions[plugin_id] = session
            await self._dispatch_loop(session, reader)
        except (asyncio.IncompleteReadError, ConnectionResetError):
            log.info("plugin_ipc_client_disconnected", plugin_id=plugin_id)
        except FrameError as exc:
            log.warning(
                "plugin_ipc_frame_error",
                plugin_id=plugin_id,
                error=str(exc),
            )
        except Exception as exc:  # noqa: BLE001
            log.error(
                "plugin_ipc_unhandled",
                plugin_id=plugin_id,
                error=str(exc),
                error_type=type(exc).__name__,
            )
        finally:
            # Identity-checked teardown: only remove + release when OUR session is
            # still the registered one. On a reconnect overlap the old connection's
            # teardown must not evict the successor's session (and release its
            # live reservations). `session` is None if the handshake failed, in
            # which case this connection registered nothing.
            if session is not None and self._sessions.get(plugin_id) is session:
                self._sessions.pop(plugin_id, None)
                self._release_session_resources(session)
                # Fail any in-flight host->plugin invokes so no caller hangs past
                # the session (mirrors the Rust host dropping its pending map).
                for fut in session.pending_invokes.values():
                    if not fut.done():
                        fut.set_exception(
                            _RpcError(f"plugin_disconnected: {plugin_id}")
                        )
                session.pending_invokes.clear()
            writer.close()
            try:
                await writer.wait_closed()
            except Exception:  # noqa: BLE001
                pass

    async def _handshake(
        self,
        plugin_id: str,
        reader: asyncio.StreamReader,
        writer: asyncio.StreamWriter,
    ) -> PluginSession | None:
        env = await read_frame(reader)
        if env is None or env.method != "hello":
            await self._send_error(writer, "expected hello envelope", req_id="-")
            return None
        try:
            token = CapabilityToken.from_string(env.token)
            self._token_issuer.verify(token)
        except TokenError as exc:
            await self._send_error(
                writer, f"capability token invalid: {exc}", req_id=env.request_id
            )
            return None
        if token.plugin_id != plugin_id:
            await self._send_error(
                writer,
                f"token plugin_id {token.plugin_id} does not match socket {plugin_id}",
                req_id=env.request_id,
            )
            return None
        await self._send_response(writer, env.request_id, {"ready": True})
        return PluginSession(
            plugin_id=plugin_id,
            token=token,
            writer=writer,
            subscriptions=set(),
            mavlink_subscriptions=set(),
            pump_tasks=[],
        )

    async def _dispatch_loop(
        self, session: PluginSession, reader: asyncio.StreamReader
    ) -> None:
        while True:
            env = await read_frame(reader)
            if env is None:
                return
            # A response envelope is the plugin replying to a host-issued
            # tool.invoke; resolve THIS session's pending future and move on (it
            # is not a request to dispatch). Only this session's own map is
            # consulted, so a session cannot resolve another session's waiter.
            if env.type == "response":
                fut = session.pending_invokes.pop(env.request_id, None)
                if fut is not None and not fut.done():
                    fut.set_result(env)
                continue
            # Re-check token freshness on every request. The handshake
            # accepted the token once; the session lives longer than the
            # token's TTL is allowed to. If the token has aged past
            # expires_at, refuse to route and signal token_expired so the
            # runner can request a fresh token from the supervisor.
            if session.token.is_expired():
                await self._send_error(
                    session.writer,
                    "token_expired",
                    req_id=env.request_id,
                )
                continue
            spec = _METHOD_HANDLERS.get(env.method)
            if spec is None:
                await self._send_error(
                    session.writer,
                    f"unknown method {env.method}",
                    req_id=env.request_id,
                )
                continue
            handler, requires = spec
            if requires is not None and requires not in session.token.granted_caps:
                await self._send_error(
                    session.writer,
                    f"capability_denied: {requires}",
                    req_id=env.request_id,
                )
                continue
            try:
                result = await handler(self, session, env)
                await self._send_response(session.writer, env.request_id, result)
            except _RpcError as exc:
                await self._send_error(
                    session.writer, str(exc), req_id=env.request_id
                )
            except _CapabilityDenied as exc:
                # Handler-level inline gate (mavlink.register_component,
                # peripheral.register_driver, pose-inject classification,
                # vio-component classification). Surface as the same
                # capability_denied envelope the dispatch-level gate
                # uses, so the client maps both consistently.
                await self._send_error(
                    session.writer,
                    f"capability_denied: {exc.capability}",
                    req_id=env.request_id,
                )
            except _AllowlistViolation as exc:
                await self._send_error(
                    session.writer,
                    f"allowlist_violation: {exc.basename}",
                    req_id=env.request_id,
                )

    # ---- handlers ----------------------------------------------------

    async def _handle_event_publish(
        self, session: PluginSession, env: Envelope
    ) -> dict:
        topic = env.args.get("topic")
        payload = env.args.get("payload") or {}
        if not isinstance(topic, str):
            raise _RpcError("topic must be a string")
        if not is_publish_allowed(
            plugin_id=session.plugin_id,
            topic=topic,
            granted_caps=set(session.token.granted_caps),
        ):
            raise _RpcError(f"publish not permitted on topic {topic}")
        evt = Event(
            topic=topic,
            timestamp_ms=now_ms(),
            publisher_plugin_id=session.plugin_id,
            payload=payload if isinstance(payload, dict) else {},
        )
        delivered = await self._bus.publish(evt)
        return {"delivered": delivered}

    async def _handle_event_subscribe(
        self, session: PluginSession, env: Envelope
    ) -> dict:
        topic_pattern = env.args.get("topic")
        if not isinstance(topic_pattern, str):
            raise _RpcError("topic must be a string")
        if not is_subscribe_allowed(
            plugin_id=session.plugin_id,
            topic_pattern=topic_pattern,
            granted_caps=set(session.token.granted_caps),
        ):
            raise _RpcError(f"subscribe not permitted on {topic_pattern}")
        if topic_pattern in session.subscriptions:
            return {"already_subscribed": True}
        session.subscriptions.add(topic_pattern)
        # Spawn a fan-out task that pushes events to the plugin.
        pump = asyncio.create_task(
            self._pump_subscription(session, topic_pattern)
        )
        pump.add_done_callback(log_task_exceptions)
        return {"subscribed": True}

    async def _pump_subscription(
        self, session: PluginSession, topic_pattern: str
    ) -> None:
        try:
            async for evt in self._bus.subscribe(topic_pattern):
                if topic_pattern not in session.subscriptions:
                    return
                env = Envelope(
                    type="event",
                    method="event.deliver",
                    capability="event.subscribe",
                    args={
                        "topic": evt.topic,
                        "payload": evt.payload,
                        "publisher": evt.publisher_plugin_id,
                        "timestamp_ms": evt.timestamp_ms,
                    },
                    request_id=f"evt-{evt.timestamp_ms}",
                    token=session.token.to_string(),
                )
                try:
                    session.writer.write(encode_frame(env))
                    await session.writer.drain()
                except (ConnectionError, BrokenPipeError):
                    return
        except asyncio.CancelledError:
            return

    async def _handle_ping(
        self, session: PluginSession, env: Envelope
    ) -> dict:
        return {"pong": True, "plugin_id": session.plugin_id}

    # ---- gated stubs (await full host service wiring) ----------------
    # Some surfaces still defer to the underlying host service. Each
    # method gate is declared in _METHOD_HANDLERS; the dispatcher
    # rejects ungranted callers with capability_denied before the
    # handler runs. The bodies below return not_implemented until the
    # corresponding host service exposes a stable hook.

    async def _handle_telemetry_subscribe(
        self, session: PluginSession, env: Envelope
    ) -> dict:
        return {"error": "not_implemented", "method": "telemetry.subscribe"}

    async def _handle_mission_read(
        self, session: PluginSession, env: Envelope
    ) -> dict:
        return {"error": "not_implemented", "method": "mission.read"}

    async def _handle_mission_write(
        self, session: PluginSession, env: Envelope
    ) -> dict:
        return {"error": "not_implemented", "method": "mission.write"}

    async def _handle_recording_start(
        self, session: PluginSession, env: Envelope
    ) -> dict:
        return {"error": "not_implemented", "method": "recording.start"}

    async def _handle_recording_stop(
        self, session: PluginSession, env: Envelope
    ) -> dict:
        return {"error": "not_implemented", "method": "recording.stop"}

    # ---- session resource cleanup -----------------------------------

    def _release_session_resources(self, session: PluginSession) -> None:
        """Drop all per-session host resources when the plugin disconnects.

        Each handler family stores per-plugin state on a host-services
        facade (component reservations, driver registrations, camera
        claims, telemetry channels, in-memory config). We release them
        symmetrically so a crashed-and-restarted plugin does not leak
        stale registrations into the host's view of the world.
        """
        plugin_id = session.plugin_id
        # Cancel any MAVLink subscription pumps the dispatcher spawned.
        for task in session.pump_tasks:
            task.cancel()
        session.pump_tasks.clear()
        try:
            self._host.components.release_plugin(plugin_id)
            self._host.drivers.release_plugin(plugin_id)
            self._host.cameras.release_plugin(plugin_id)
            self._host.telemetry.clear_plugin(plugin_id)
        except Exception as exc:  # noqa: BLE001
            log.warning(
                "plugin_ipc_release_resources_failed",
                plugin_id=plugin_id,
                error=str(exc),
            )

    # ---- MAVLink subscription pump ----------------------------------

    def spawn_mavlink_pump(
        self, session: PluginSession, msg_name: str
    ) -> None:
        """Subscribe the plugin to MAVLink frames matching ``msg_name``.

        Delegates to :mod:`ados.plugins.ipc.mavlink_pump` so this module
        does not carry the pump body. The pump consumes the raw byte
        queue exposed by the host's MAVLink connection and forwards
        each frame to the plugin as an event-style envelope.
        """
        from ados.plugins.ipc import mavlink_pump as _pump

        _pump.spawn_pump(self, session, msg_name)

    # ---- helpers -----------------------------------------------------

    async def _send_response(
        self,
        writer: asyncio.StreamWriter,
        request_id: str,
        result: dict,
    ) -> None:
        env = Envelope(
            type="response",
            method="response",
            capability="",
            args=result,
            request_id=request_id,
            token="",
        )
        writer.write(encode_frame(env))
        await writer.drain()

    async def _send_error(
        self,
        writer: asyncio.StreamWriter,
        message: str,
        req_id: str,
    ) -> None:
        env = Envelope(
            type="response",
            method="response",
            capability="",
            args={},
            request_id=req_id,
            token="",
            error=message,
        )
        writer.write(encode_frame(env))
        await writer.drain()

    async def invoke_tool(
        self,
        plugin_id: str,
        tool_name: str,
        arguments: dict,
        *,
        timeout_s: float = 5.0,
    ) -> dict:
        """Ask a running plugin to run one of its declared tools; return the
        result dict. Raises when the plugin is not connected, lacks the cap
        the generated table names for ``tool.invoke`` (``mcp.expose``), the
        call times out, or the plugin replies with an error.

        This is a host-side PRE-CHECK, not the sole authority: the session token
        was verified at handshake so ``session.token.granted_caps`` is a
        trustworthy gate, and the runner re-checks against the same generated
        table so the two sides cannot drift. The pending reply is correlated on
        the session's OWN map, so only this plugin's response frame can resolve
        it. (In production the Rust plugin host owns the control socket; this
        Python path is the fallback host's equivalent.)
        """
        session = self._sessions.get(plugin_id)
        if session is None:
            raise _RpcError(f"plugin_not_running: {plugin_id}")
        # Re-check expiry, like the dispatch loop does per request: the session can
        # outlive the token's TTL, so an invoke against an aged-out token is refused.
        if session.token.is_expired():
            raise _RpcError("token_expired")
        required = REQUIRED_CAP.get("tool.invoke")
        if required is not None and required not in session.token.granted_caps:
            raise _CapabilityDenied(plugin_id, required)
        self._invoke_seq += 1
        req_id = f"inv-{self._invoke_seq}"
        fut: asyncio.Future[Envelope] = asyncio.get_running_loop().create_future()
        session.pending_invokes[req_id] = fut
        env = Envelope(
            type="request",
            method="tool.invoke",
            capability=required or "",
            args={"tool": tool_name, "arguments": arguments},
            request_id=req_id,
            token=session.token.to_string(),
        )
        try:
            session.writer.write(encode_frame(env))
            await session.writer.drain()
        except (ConnectionError, BrokenPipeError) as exc:
            session.pending_invokes.pop(req_id, None)
            raise _RpcError(f"plugin_write_failed: {exc}") from exc
        try:
            response = await asyncio.wait_for(fut, timeout=timeout_s)
        except TimeoutError as exc:
            raise _RpcError(f"tool_timeout: {tool_name}") from exc
        finally:
            session.pending_invokes.pop(req_id, None)
        if response.error:
            raise _RpcError(response.error)
        return response.args


class _RpcError(Exception):
    """Internal: raised by handlers, converted to envelope error field."""


_HandlerFn = Callable[
    [PluginIpcServer, PluginSession, Envelope], Awaitable[dict]
]
_HandlerSpec = tuple[_HandlerFn, str | None]


# The dispatch table mapping method-name -> (handler, required-cap)
# lives in :mod:`ados.plugins.ipc.dispatch` so this module stays focused
# on the transport surface. The import is deferred to module bottom to
# avoid the cycle with :mod:`ados.plugins.ipc.handlers` (which lazy-imports
# _RpcError from this module).
from ados.plugins.ipc.dispatch import build_dispatch_table  # noqa: E402

_METHOD_HANDLERS: dict[str, _HandlerSpec] = build_dispatch_table(PluginIpcServer)
