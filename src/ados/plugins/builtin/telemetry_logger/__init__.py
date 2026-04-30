"""Built-in telemetry logger plugin.

Subscribes to every public lifecycle topic and emits a structured log
line per event. The output is designed for journald and operator-side
dashboards that tail agent logs and surface a unified event feed.

The plugin ships its manifest in code (it is a built-in entry-point,
not a packed ``.adosplug`` archive). The same lifecycle hooks
third-party plugins implement run here too, which doubles this
module as a worked example of an event-only subscriber plugin.
"""

from __future__ import annotations

from typing import Any

from ados.core.logging import get_logger
from ados.plugins.manifest import (
    AgentBlock,
    Compatibility,
    PluginManifest,
)

log = get_logger("plugin.builtin.telemetry_logger")

PLUGIN_ID = "io.altnautica.telemetry-logger"

PUBLIC_TOPICS: tuple[str, ...] = (
    "vehicle.armed",
    "vehicle.disarmed",
    "vehicle.mode_changed",
    "vehicle.battery_low",
    "vehicle.geofence_breach",
    "mission.started",
    "mission.completed",
    "mission.aborted",
    "agent.ready",
    "agent.shutdown",
)


def get_manifest() -> PluginManifest:
    """Return the manifest for the built-in telemetry logger plugin."""
    return PluginManifest(
        schema_version=1,
        id=PLUGIN_ID,
        version="0.1.0",
        name="Telemetry Logger",
        description=(
            "Subscribes to public lifecycle events and emits a structured "
            "log line per event for journald/operator dashboards."
        ),
        author="Altnautica",
        license="GPL-3.0-or-later",
        risk="low",
        compatibility=Compatibility(ados_version=">=0.9.0"),
        agent=AgentBlock(
            entrypoint="ados.plugins.builtin.telemetry_logger:TelemetryLoggerPlugin",
            isolation="inprocess",
            permissions=["event.subscribe"],
        ),
    )


# Loader probes ``manifest`` on the loaded entry-point object before falling
# back to ``get_manifest()``. Expose the manifest as a module-level attribute
# so either path works.
manifest = get_manifest()


class TelemetryLoggerPlugin:
    """Lifecycle-hook plugin class.

    The ``ctx`` argument carries the per-process IPC client and the
    plugin id, version, config, and a structlog logger. Hooks are
    awaited if they return a coroutine and skipped otherwise.
    """

    async def on_start(self, ctx: Any) -> None:
        for topic in PUBLIC_TOPICS:
            await ctx.events.subscribe(topic, self._make_callback(ctx, topic))
        ctx.log.info("telemetry_logger_started")

    async def on_stop(self, ctx: Any) -> None:
        ctx.log.info("telemetry_logger_stopped")

    @staticmethod
    def _make_callback(ctx: Any, topic: str):
        async def _callback(payload: dict[str, Any]) -> None:
            ctx.log.info("telemetry_event", topic=topic, payload=payload)

        return _callback
