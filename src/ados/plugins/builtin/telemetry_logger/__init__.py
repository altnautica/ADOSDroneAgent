"""Built-in telemetry logger plugin.

Subscribes to every public lifecycle topic and emits a structured log
line per event. The output is designed for journald and operator-side
dashboards that tail agent logs and surface a unified event feed.

The plugin ships with the agent package: its manifest is the
``manifest.yaml`` beside this module, which the native plugin lifecycle
installs as a subprocess plugin that the plugin host serves through the
shared runner, exactly like a third-party plugin. The same lifecycle hooks
third-party plugins implement run here too, which doubles this
module as a worked example of an event-only subscriber plugin.
"""

from __future__ import annotations

from collections.abc import Awaitable, Callable
from typing import Any

from ados.core.logging import get_logger

log = get_logger("plugin.builtin.telemetry_logger")

PLUGIN_ID = "io.altnautica.telemetry-logger"

PUBLIC_TOPICS: tuple[str, ...] = (
    "vehicle.armed",
    "vehicle.disarmed",
    "vehicle.mode_changed",
    "vehicle.battery_low",
    "vehicle.geofence_breach",
    "agent.ready",
    "agent.shutdown",
)


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
    def _make_callback(ctx: Any, topic: str) -> Callable[[dict[str, Any]], Awaitable[None]]:
        async def _callback(payload: dict[str, Any]) -> None:
            ctx.log.info("telemetry_event", topic=topic, payload=payload)

        return _callback
