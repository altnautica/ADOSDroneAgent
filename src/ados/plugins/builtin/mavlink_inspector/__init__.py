"""Built-in MAVLink inspector plugin.

Tracks the most recent vehicle state changes (arm state, flight mode,
battery percent, last event timestamp) and republishes a snapshot
under its own ``plugin.<id>.snapshot`` namespace so diagnostic UIs
have a single canonical state object to render.

The plugin ships its manifest in code (it is a built-in entry-point,
not a packed ``.adosplug`` archive). The same lifecycle hooks
third-party plugins implement run here too, which doubles this
module as a worked example of an aggregator plugin that combines
``event.subscribe`` and ``event.publish``.
"""

from __future__ import annotations

import time
from typing import Any

from ados.core.logging import get_logger
from ados.plugins.manifest import (
    AgentBlock,
    Compatibility,
    PluginManifest,
)

log = get_logger("plugin.builtin.mavlink_inspector")

PLUGIN_ID = "io.altnautica.mavlink-inspector"

SUBSCRIBED_TOPICS: tuple[str, ...] = (
    "vehicle.armed",
    "vehicle.disarmed",
    "vehicle.mode_changed",
    "vehicle.battery_low",
    "agent.ready",
)


def get_manifest() -> PluginManifest:
    """Return the manifest for the built-in MAVLink inspector plugin."""
    return PluginManifest(
        schema_version=1,
        id=PLUGIN_ID,
        version="0.1.0",
        name="MAVLink Inspector",
        description=(
            "Subscribes to vehicle state changes and republishes a snapshot "
            "under plugin.<id>.snapshot for diagnostic UIs."
        ),
        author="Altnautica",
        license="GPL-3.0-or-later",
        risk="low",
        compatibility=Compatibility(ados_version=">=0.9.0"),
        agent=AgentBlock(
            entrypoint="ados.plugins.builtin.mavlink_inspector:MavlinkInspectorPlugin",
            isolation="inprocess",
            permissions=["event.subscribe", "event.publish"],
        ),
    )


# Loader probes ``manifest`` on the loaded entry-point object before falling
# back to ``get_manifest()``. Expose the manifest as a module-level attribute
# so either path works.
manifest = get_manifest()


class MavlinkInspectorPlugin:
    """Lifecycle-hook plugin class.

    Maintains an in-memory dict of last-seen vehicle state and
    republishes the full snapshot every time an observed event
    arrives. The state dict starts with all fields set to ``None``
    so consumers can render a stable shape on first read.
    """

    def __init__(self) -> None:
        self._state: dict[str, Any] = {
            "armed": None,
            "mode": None,
            "battery_pct": None,
            "last_event_ts_ms": None,
        }

    async def on_start(self, ctx: Any) -> None:
        for topic in SUBSCRIBED_TOPICS:
            await ctx.events.subscribe(topic, self._make_callback(ctx, topic))
        ctx.log.info("mavlink_inspector_started")

    async def on_stop(self, ctx: Any) -> None:
        ctx.log.info("mavlink_inspector_stopped")

    def _apply(self, topic: str, payload: dict[str, Any]) -> None:
        """Fold an incoming event into the running state snapshot."""
        if topic == "vehicle.armed":
            self._state["armed"] = True
        elif topic == "vehicle.disarmed":
            self._state["armed"] = False
        elif topic == "vehicle.mode_changed":
            mode = payload.get("mode")
            if isinstance(mode, str):
                self._state["mode"] = mode
        elif topic == "vehicle.battery_low":
            pct = payload.get("battery_pct")
            if isinstance(pct, (int, float)):
                self._state["battery_pct"] = float(pct)
        # agent.ready falls through with a timestamp refresh only.
        self._state["last_event_ts_ms"] = int(time.time() * 1000)

    def _make_callback(self, ctx: Any, topic: str):
        async def _callback(payload: dict[str, Any]) -> None:
            self._apply(topic, payload)
            await ctx.events.publish(
                f"plugin.{PLUGIN_ID}.snapshot",
                dict(self._state),
            )

        return _callback
