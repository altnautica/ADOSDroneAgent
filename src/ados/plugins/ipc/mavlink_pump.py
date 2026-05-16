"""MAVLink subscription pump.

Bridges the host's FC byte queue to a plugin's IPC session. The plugin
subscribed via ``ctx.mavlink.subscribe(msg_name, handler)``; the
dispatcher seeds a pump task that pulls bytes off the queue, wraps
each frame in an event envelope, and writes it back through the
plugin's UDS connection.

Separated from :mod:`ados.plugins.ipc_server` to keep that module's
total line count under control. The pump only depends on the
public-ish surface of PluginSession + HostServices.mavlink.
"""

from __future__ import annotations

import asyncio
from typing import TYPE_CHECKING

from ados.core.asyncio_util import log_task_exceptions
from ados.core.logging import get_logger
from ados.plugins.events import now_ms
from ados.plugins.rpc import Envelope, encode_frame

if TYPE_CHECKING:
    from ados.plugins.ipc.host_services import MAVLinkRouter
    from ados.plugins.ipc_server import PluginIpcServer, PluginSession

log = get_logger("plugins.ipc.mavlink_pump")


def spawn_pump(server: "PluginIpcServer", session: "PluginSession", msg_name: str) -> None:
    """Subscribe the plugin to MAVLink frames matching ``msg_name``."""
    router = server.host.mavlink
    if router is None:
        log.debug(
            "plugin_mavlink_subscribe_router_missing",
            plugin_id=session.plugin_id,
            msg_name=msg_name,
        )
        return
    try:
        queue = router.subscribe()
    except Exception as exc:  # noqa: BLE001
        log.warning(
            "plugin_mavlink_subscribe_failed",
            plugin_id=session.plugin_id,
            msg_name=msg_name,
            error=str(exc),
        )
        return
    task = asyncio.create_task(_pump_loop(session, msg_name, queue, router))
    task.add_done_callback(log_task_exceptions)
    session.pump_tasks.append(task)


async def _pump_loop(
    session: "PluginSession",
    msg_name: str,
    queue: asyncio.Queue,
    router: "MAVLinkRouter",
) -> None:
    try:
        while True:
            if msg_name not in session.mavlink_subscriptions:
                return
            try:
                payload = await queue.get()
            except asyncio.CancelledError:
                return
            frame_bytes = payload
            if not isinstance(frame_bytes, (bytes, bytearray)):
                try:
                    frame_bytes = bytes(payload or b"")
                except (TypeError, ValueError):
                    frame_bytes = b""
            env = Envelope(
                type="event",
                method="mavlink.deliver",
                capability="mavlink.read",
                args={
                    "msg_name": msg_name,
                    "frame": bytes(frame_bytes),
                    "timestamp_ms": now_ms(),
                },
                request_id=f"mav-{now_ms()}",
                token=session.token.to_string(),
            )
            try:
                session.writer.write(encode_frame(env))
                await session.writer.drain()
            except (ConnectionError, BrokenPipeError):
                return
    except asyncio.CancelledError:
        return
    finally:
        try:
            router.unsubscribe(queue)
        except Exception:  # noqa: BLE001
            pass


__all__ = ["spawn_pump"]
