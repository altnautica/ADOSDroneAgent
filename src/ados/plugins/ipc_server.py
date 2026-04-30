"""Supervisor-side IPC server.

Binds a Unix domain socket per plugin at ``/run/ados/plugins/<id>.sock``.
The plugin runner connects, presents its capability token, and then
issues request envelopes (event publish/subscribe, MAVLink read/write,
HAL access, etc). Each request is validated against the token's
declared capability set before routing to the host service.

Currently routes only the event surface (``event.publish``,
``event.subscribe``). MAVLink, HAL, and telemetry extension routing
get wired as additional handlers once the supervisor exposes stable
hooks into those services.
"""

from __future__ import annotations

import asyncio
import os
from dataclasses import dataclass
from pathlib import Path
from typing import Awaitable, Callable

from ados.core.logging import get_logger
from ados.plugins.events import (
    Event,
    EventBus,
    is_publish_allowed,
    is_subscribe_allowed,
    now_ms,
)
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


@dataclass
class PluginSession:
    plugin_id: str
    token: CapabilityToken
    writer: asyncio.StreamWriter
    subscriptions: set[str]


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
    ) -> None:
        self._bus = bus
        self._token_issuer = token_issuer
        self._socket_dir = socket_dir
        self._servers: dict[str, asyncio.AbstractServer] = {}
        self._sessions: dict[str, PluginSession] = {}

    async def start_for_plugin(self, plugin_id: str) -> Path:
        """Start a UDS server for one plugin. Returns the socket path."""
        self._socket_dir.mkdir(parents=True, exist_ok=True)
        sock_path = self._socket_dir / f"{plugin_id}.sock"
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
        if server is not None:
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
            writer.close()
            try:
                await writer.wait_closed()
            except Exception:  # noqa: BLE001
                pass
            self._sessions.pop(plugin_id, None)

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
        )

    async def _dispatch_loop(
        self, session: PluginSession, reader: asyncio.StreamReader
    ) -> None:
        while True:
            env = await read_frame(reader)
            if env is None:
                return
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
        asyncio.create_task(
            self._pump_subscription(session, topic_pattern)
        )
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

    # ---- gated stubs -------------------------------------------------
    # The next batch of capability-gated surfaces. Each method gate is
    # declared in _METHOD_HANDLERS; the dispatcher rejects ungranted
    # callers with capability_denied before reaching the handler. The
    # bodies return not_implemented until the corresponding host
    # service exposes a stable hook.

    async def _handle_telemetry_subscribe(
        self, session: PluginSession, env: Envelope
    ) -> dict:
        return {"error": "not_implemented", "method": "telemetry.subscribe"}

    async def _handle_telemetry_extend(
        self, session: PluginSession, env: Envelope
    ) -> dict:
        return {"error": "not_implemented", "method": "telemetry.extend"}

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

    async def _handle_mavlink_subscribe(
        self, session: PluginSession, env: Envelope
    ) -> dict:
        return {"error": "not_implemented", "method": "mavlink.subscribe"}

    async def _handle_mavlink_send(
        self, session: PluginSession, env: Envelope
    ) -> dict:
        return {"error": "not_implemented", "method": "mavlink.send"}

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


class _RpcError(Exception):
    """Internal: raised by handlers, converted to envelope error field."""


_HandlerFn = Callable[
    [PluginIpcServer, PluginSession, Envelope], Awaitable[dict]
]

# Method dispatch table. Each entry is (handler, required_capability).
# A required_capability of None means the method is either ungated or
# gated inline by the handler itself (event.publish and event.subscribe
# do their own per-topic checks via is_publish_allowed / is_subscribe_allowed).
# Methods with a non-None requirement are rejected with capability_denied
# before reaching the handler when the calling token does not carry the cap.
_HandlerSpec = tuple[_HandlerFn, str | None]
_METHOD_HANDLERS: dict[str, _HandlerSpec] = {
    "event.publish": (PluginIpcServer._handle_event_publish, None),
    "event.subscribe": (PluginIpcServer._handle_event_subscribe, None),
    "ping": (PluginIpcServer._handle_ping, None),
    "telemetry.subscribe": (
        PluginIpcServer._handle_telemetry_subscribe,
        "telemetry.read",
    ),
    "telemetry.extend": (
        PluginIpcServer._handle_telemetry_extend,
        "telemetry.extend",
    ),
    "mission.read": (PluginIpcServer._handle_mission_read, "mission.read"),
    "mission.write": (PluginIpcServer._handle_mission_write, "mission.write"),
    "recording.start": (
        PluginIpcServer._handle_recording_start,
        "recording.write",
    ),
    "recording.stop": (
        PluginIpcServer._handle_recording_stop,
        "recording.write",
    ),
    "mavlink.subscribe": (
        PluginIpcServer._handle_mavlink_subscribe,
        "mavlink.read",
    ),
    "mavlink.send": (PluginIpcServer._handle_mavlink_send, "mavlink.write"),
}
