"""Built-in geofence plugin.

Subscribes to ``vehicle.geofence_breach`` (a public host topic) and
republishes a structured alert under its own ``plugin.<id>.alert``
namespace so other plugins, the GCS, and operator-facing log streams
have one canonical alert shape to render.

The plugin ships its manifest in code (it is a built-in entry-point,
not a packed ``.adosplug`` archive). The same lifecycle hooks
third-party plugins implement run here too, which doubles this
module as the simplest worked example for the SDK.
"""

from __future__ import annotations

from typing import Any

from ados.core.logging import get_logger
from ados.plugins.manifest import (
    AgentBlock,
    Compatibility,
    PluginManifest,
)

log = get_logger("plugin.builtin.geofence")

PLUGIN_ID = "io.altnautica.geofence"


def get_manifest() -> PluginManifest:
    """Return the manifest for the built-in geofence plugin."""
    return PluginManifest(
        schema_version=1,
        id=PLUGIN_ID,
        version="0.1.0",
        name="Geofence",
        description=(
            "Republishes geofence breaches under a stable plugin namespace "
            "so other plugins and the GCS render them consistently."
        ),
        author="Altnautica",
        license="GPL-3.0-or-later",
        risk="low",
        compatibility=Compatibility(ados_version=">=0.9.0"),
        agent=AgentBlock(
            entrypoint="ados.plugins.builtin.geofence:GeofencePlugin",
            isolation="inprocess",
            permissions=["event.subscribe", "event.publish"],
        ),
    )


# Loader probes ``manifest`` on the loaded entry-point object before falling
# back to ``get_manifest()``. Expose the manifest as a module-level attribute
# so either path works.
manifest = get_manifest()


class GeofencePlugin:
    """Lifecycle-hook plugin class.

    The ``ctx`` argument carries the per-process IPC client and the
    plugin id, version, config, and a structlog logger. Hooks are
    awaited if they return a coroutine and skipped otherwise.
    """

    async def on_start(self, ctx: Any) -> None:
        async def _on_breach(payload: dict[str, Any]) -> None:
            ctx.log.info(
                "geofence_breach_observed",
                fence=payload.get("fence"),
                severity=payload.get("severity", "warning"),
            )
            await ctx.events.publish(
                f"plugin.{PLUGIN_ID}.alert",
                {
                    "kind": "geofence_breach",
                    "fence": payload.get("fence"),
                    "severity": payload.get("severity", "warning"),
                    "lat": payload.get("lat"),
                    "lon": payload.get("lon"),
                    "alt_m_agl": payload.get("alt_m_agl"),
                },
            )

        await ctx.events.subscribe("vehicle.geofence_breach", _on_breach)
        ctx.log.info("geofence_plugin_started")

    async def on_stop(self, ctx: Any) -> None:
        ctx.log.info("geofence_plugin_stopped")
