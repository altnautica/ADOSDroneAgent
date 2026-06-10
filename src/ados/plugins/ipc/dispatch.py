"""Dispatch table for the plugin IPC server.

Separated from :mod:`ados.plugins.ipc_server` so the server module
stays focused on the transport surface (UDS server, handshake, frame
read/write). The table maps method name to (handler, required-cap).

The required-cap for every method is NOT spelled here. It is looked up
from the generated :data:`ados.plugins._dispatch_generated.REQUIRED_CAP`
table, the single source of truth shared with the Rust host (the source
file is the ``[[method]]`` section of
``crates/ados-protocol/capabilities.toml``). Pairing each handler with
its generated cap means a method can never be registered without its
gate, and the Rust and Python hosts cannot drift. ``None`` for a method's
required-cap means it is either ungated (the event surface and ``ping``)
or gated inline by the handler itself.
"""

from __future__ import annotations

from ados.plugins._dispatch_generated import REQUIRED_CAP
from ados.plugins.ipc import handlers as _handlers


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

    Each row maps a wire method to its handler; the required cap is taken
    from the generated :data:`REQUIRED_CAP` table, never written inline,
    so it stays in lockstep with the Rust host. Registering a handler for
    a method missing from the generated table raises at import time, which
    is what stops a new (e.g. vision) handler from shipping ungated.
    """
    handlers = {
        # ---- ungated event surface (per-topic check inline) -----
        "event.publish": server_cls._handle_event_publish,
        "event.subscribe": server_cls._handle_event_subscribe,
        "ping": server_cls._handle_ping,
        # ---- telemetry ----
        "telemetry.subscribe": server_cls._handle_telemetry_subscribe,
        "telemetry.extend": _adapt(_handlers.handle_telemetry_extend),
        # ---- mission / recording (deferred host hook) ----
        "mission.read": server_cls._handle_mission_read,
        "mission.write": server_cls._handle_mission_write,
        "recording.start": server_cls._handle_recording_start,
        "recording.stop": server_cls._handle_recording_stop,
        # ---- mavlink ----
        "mavlink.subscribe": _adapt(_handlers.handle_mavlink_subscribe),
        "mavlink.send": _adapt(_handlers.handle_mavlink_send),
        # The component-kind cap is decided inline from the requested kind.
        "mavlink.register_component": _adapt(
            _handlers.handle_mavlink_register_component
        ),
        # ---- peripheral / driver / camera ----
        # The exact sensor.*.register cap depends on the driver kind; the
        # handler enforces it inline.
        "peripheral.register_driver": _adapt(
            _handlers.handle_peripheral_register_driver
        ),
        "peripheral.unregister_driver": _adapt(
            _handlers.handle_peripheral_unregister_driver
        ),
        "camera.claim": _adapt(_handlers.handle_camera_claim),
        "camera.release": _adapt(_handlers.handle_camera_release),
        "camera.get_frame": _adapt(_handlers.handle_camera_get_frame),
        # ---- config kv (per-drone / global) ----
        "config.get": _adapt(_handlers.handle_config_get),
        "config.set": _adapt(_handlers.handle_config_set),
        # ---- vendor binary spawn ----
        "process.spawn": _adapt(_handlers.handle_process_spawn),
    }
    table: dict[str, tuple] = {}
    for method, handler in handlers.items():
        if method not in REQUIRED_CAP:
            # A handler for a method the generated table does not know would
            # run with no gate. Refuse to build the table so the drift is a
            # loud import-time failure, not a silent security hole.
            raise RuntimeError(
                f"dispatch handler {method!r} is not in the generated "
                f"REQUIRED_CAP table; regenerate with "
                f"`cargo run -p ados-capabilities-codegen`"
            )
        table[method] = (handler, REQUIRED_CAP[method])
    return table


__all__ = ["build_dispatch_table"]
