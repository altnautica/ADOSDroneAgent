"""Built-in plugin discovery via Python entry-points.

Modeled on the ``ados.peripherals`` loader at
:mod:`ados.services.peripherals.loader`. First-party plugins ship as
setuptools entry-points under the ``ados.plugins`` group. Each loaded
object exposes either a ``manifest`` attribute or a ``get_manifest()``
classmethod returning a :class:`PluginManifest`.

Third-party plugins are NOT loaded here. They install from
``.adosplug`` archives into ``/var/ados/plugins/<id>/`` and the
supervisor reads their manifest off disk.
"""

from __future__ import annotations

from importlib import import_module, metadata

from ados.core.logging import get_logger
from ados.plugins.manifest import PluginManifest

log = get_logger("plugins.loader")

ENTRY_POINT_GROUP = "ados.plugins"
BUILTIN_PLUGIN_MODULES = (
    "ados.plugins.builtin.geofence",
    "ados.plugins.builtin.telemetry_logger",
    "ados.plugins.builtin.mavlink_inspector",
)


def _extract_manifest(obj: object, ep_name: str) -> PluginManifest | None:
    if isinstance(obj, PluginManifest):
        return obj
    manifest_attr = getattr(obj, "manifest", None)
    if isinstance(manifest_attr, PluginManifest):
        return manifest_attr
    getter = getattr(obj, "get_manifest", None)
    if callable(getter):
        try:
            result = getter()
        except Exception as exc:  # noqa: BLE001
            log.warning(
                "plugin_entry_point_getter_failed",
                entry_point=ep_name,
                error=str(exc),
            )
            return None
        if isinstance(result, PluginManifest):
            return result
        log.warning(
            "plugin_entry_point_bad_return",
            entry_point=ep_name,
            returned_type=type(result).__name__,
        )
        return None
    log.warning(
        "plugin_entry_point_no_manifest",
        entry_point=ep_name,
        loaded_type=type(obj).__name__,
    )
    return None


def load_builtin_manifests() -> list[PluginManifest]:
    """Load manifests for every entry-point under ``ados.plugins``.

    Gracefully handles missing group, import errors per entry, and
    malformed plugins. One bad plugin does not kill the rest.
    """
    manifests: list[PluginManifest] = []
    try:
        eps = metadata.entry_points()
        group = (
            eps.select(group=ENTRY_POINT_GROUP)
            if hasattr(eps, "select")
            else []
        )
    except Exception as exc:  # noqa: BLE001
        log.warning("plugin_entry_points_lookup_failed", error=str(exc))
        return manifests

    seen_ids: set[str] = set()

    for ep in group:
        try:
            loaded = ep.load()
        except Exception as exc:  # noqa: BLE001
            log.warning(
                "plugin_entry_point_load_failed",
                entry_point=ep.name,
                error=str(exc),
            )
            continue

        manifest = _extract_manifest(loaded, ep.name)
        if manifest is not None and manifest.id not in seen_ids:
            manifests.append(manifest)
            seen_ids.add(manifest.id)
            log.info(
                "plugin_entry_point_loaded",
                entry_point=ep.name,
                plugin_id=manifest.id,
            )

    for module_name in BUILTIN_PLUGIN_MODULES:
        try:
            module = import_module(module_name)
        except Exception as exc:  # noqa: BLE001
            log.warning(
                "plugin_builtin_fallback_load_failed",
                module=module_name,
                error=str(exc),
            )
            continue

        manifest = _extract_manifest(module, module_name)
        if manifest is not None and manifest.id not in seen_ids:
            manifests.append(manifest)
            seen_ids.add(manifest.id)
            log.info(
                "plugin_builtin_fallback_loaded",
                module=module_name,
                plugin_id=manifest.id,
            )

    return manifests
