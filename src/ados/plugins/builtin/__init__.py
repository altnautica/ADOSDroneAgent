"""First-party plugins that ship inside the agent package.

Each one is an ordinary subprocess plugin: its manifest is defined in code,
``PluginSupervisor.install_builtin`` writes it into the plugin install
directory and records the install, and from then on the plugin host serves
it and systemd runs it through ``ados-plugin-runner`` like any third-party
plugin. Nothing runs in the agent's own processes.
"""

from __future__ import annotations

from collections.abc import Callable

from ados.plugins.builtin import geofence, mavlink_inspector, telemetry_logger
from ados.plugins.manifest import PluginManifest

_BUILTINS: dict[str, Callable[[], PluginManifest]] = {
    geofence.PLUGIN_ID: geofence.get_manifest,
    mavlink_inspector.PLUGIN_ID: mavlink_inspector.get_manifest,
    telemetry_logger.PLUGIN_ID: telemetry_logger.get_manifest,
}


def builtin_manifests() -> dict[str, PluginManifest]:
    """Every built-in plugin's manifest, keyed by plugin id."""
    return {plugin_id: build() for plugin_id, build in _BUILTINS.items()}


def builtin_manifest(plugin_id: str) -> PluginManifest | None:
    """The manifest of built-in ``plugin_id``, or None when it is not one."""
    build = _BUILTINS.get(plugin_id)
    return build() if build is not None else None
