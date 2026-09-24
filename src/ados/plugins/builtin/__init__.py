"""First-party plugins that ship inside the agent package.

Each one is an ordinary subprocess plugin whose ``manifest.yaml`` sits beside
its module. The native plugin lifecycle (``POST /api/plugins/install_builtin``)
embeds those same files, writes the manifest into the plugin install
directory and records the install; from then on the plugin host serves it and
systemd runs it through ``ados-plugin-runner`` like any third-party plugin.
Nothing runs in the agent's own processes.
"""

from __future__ import annotations

from importlib.resources import files

from ados.plugins.builtin import geofence, mavlink_inspector, telemetry_logger
from ados.plugins.manifest import PluginManifest

# Plugin id -> the subpackage holding its module and manifest.yaml.
_BUILTINS: dict[str, str] = {
    geofence.PLUGIN_ID: "geofence",
    mavlink_inspector.PLUGIN_ID: "mavlink_inspector",
    telemetry_logger.PLUGIN_ID: "telemetry_logger",
}


def _load(package: str) -> PluginManifest:
    text = (files(__name__) / package / "manifest.yaml").read_text(encoding="utf-8")
    return PluginManifest.from_yaml_text(text)


def builtin_manifest(plugin_id: str) -> PluginManifest | None:
    """The manifest of built-in ``plugin_id``, or None when it is not one."""
    package = _BUILTINS.get(plugin_id)
    return _load(package) if package is not None else None


def builtin_manifests() -> dict[str, PluginManifest]:
    """Every built-in plugin's manifest, keyed by plugin id."""
    return {plugin_id: _load(package) for plugin_id, package in _BUILTINS.items()}
