"""Dispatch table for the plugin IPC server.

Separated from :mod:`ados.plugins.ipc_server` so the server module
stays focused on the transport surface (UDS server, handshake, frame
read/write). The table maps method name to (handler, required-cap).
``None`` for required-cap means the method is either ungated or gated
inline by the handler itself.
"""

from __future__ import annotations

from typing import TYPE_CHECKING

from ados.plugins.ipc import handlers as _handlers

if TYPE_CHECKING:
    from ados.plugins.ipc_server import PluginIpcServer, PluginSession


def _adapt(fn):
    """Wrap a standalone handler so it matches the (server, session, env)
    signature the dispatch loop expects.

    Bound methods on PluginIpcServer use the same shape, so the table
    can mix and match.
    """

    async def _bound(server, session, env):
        return await fn(server, session, env)

    return _bound


def build_dispatch_table(server_cls) -> dict[str, tuple]:
    """Return a fresh dispatch table bound to ``server_cls``.

    Built as a factory so tests can subclass PluginIpcServer to inject
    extra handlers without rewriting the table.
    """
    return {
        # ---- ungated event surface (per-topic check inline) -----
        "event.publish": (server_cls._handle_event_publish, None),
        "event.subscribe": (server_cls._handle_event_subscribe, None),
        "ping": (server_cls._handle_ping, None),
        # ---- telemetry ----
        "telemetry.subscribe": (
            server_cls._handle_telemetry_subscribe,
            "telemetry.read",
        ),
        "telemetry.extend": (
            _adapt(_handlers.handle_telemetry_extend),
            "telemetry.extend",
        ),
        # ---- mission / recording (deferred host hook) ----
        "mission.read": (server_cls._handle_mission_read, "mission.read"),
        "mission.write": (server_cls._handle_mission_write, "mission.write"),
        "recording.start": (
            server_cls._handle_recording_start,
            "recording.write",
        ),
        "recording.stop": (
            server_cls._handle_recording_stop,
            "recording.write",
        ),
        # ---- mavlink ----
        "mavlink.subscribe": (
            _adapt(_handlers.handle_mavlink_subscribe),
            "mavlink.read",
        ),
        "mavlink.send": (
            _adapt(_handlers.handle_mavlink_send),
            "mavlink.write",
        ),
        "mavlink.register_component": (
            _adapt(_handlers.handle_mavlink_register_component),
            # Gating on the matching component-kind cap is performed
            # inline; the cap id depends on the requested kind.
            None,
        ),
        # ---- peripheral / driver / camera ----
        "peripheral.register_driver": (
            _adapt(_handlers.handle_peripheral_register_driver),
            # The exact sensor.*.register cap depends on the driver
            # kind; the handler enforces it inline.
            None,
        ),
        "peripheral.unregister_driver": (
            _adapt(_handlers.handle_peripheral_unregister_driver),
            None,
        ),
        "camera.claim": (
            _adapt(_handlers.handle_camera_claim),
            "sensor.camera.register",
        ),
        "camera.release": (
            _adapt(_handlers.handle_camera_release),
            "sensor.camera.register",
        ),
        "camera.get_frame": (
            _adapt(_handlers.handle_camera_get_frame),
            "sensor.camera.register",
        ),
        # ---- config kv (per-drone / global) ----
        "config.get": (_adapt(_handlers.handle_config_get), None),
        "config.set": (_adapt(_handlers.handle_config_set), None),
        # ---- vendor binary spawn ----
        "process.spawn": (
            _adapt(_handlers.handle_process_spawn),
            "process.spawn",
        ),
    }


__all__ = ["build_dispatch_table"]
