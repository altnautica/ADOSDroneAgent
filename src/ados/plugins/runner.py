"""Plugin subprocess runner.

This is the program systemd starts inside the per-plugin
``ados-plugin-<id>.service`` unit. The supervisor passes the plugin
id; the runner reads the unpacked plugin directory at
``/var/ados/plugins/<id>/``, imports the entry-point, hands it a
:class:`PluginContext`, and runs lifecycle hooks until shutdown.

Initial baseline ships the runner shape and the lifecycle skeleton.
The real Unix-domain-socket bridge to the supervisor (capability
tokens, event publishing, MAVLink read/write) lands as the IPC
client wiring once the supervisor IPC server exposes the matching
handlers.

Exit codes:
* 0 graceful shutdown
* 1 plugin error (lifecycle hook raised)
* 2 unable to load manifest or entry-point
* 3 SIGTERM honored
"""

from __future__ import annotations

import asyncio
import importlib
import signal
import sys
from pathlib import Path

import click

from ados.core.logging import configure_logging, get_logger
from ados.core.paths import PLUGINS_INSTALL_DIR
from ados.plugins.archive import MANIFEST_FILENAME
from ados.plugins.errors import ManifestError, PluginError
from ados.plugins.manifest import PluginManifest

log = get_logger("plugins.runner")


def _load_plugin_class(install_dir: Path, manifest: PluginManifest):
    if manifest.agent is None:
        raise ManifestError("plugin has no agent half; nothing to run")
    entrypoint = manifest.agent.entrypoint
    if ":" in entrypoint:
        # ``module:Class`` style for built-in entry-points.
        mod_name, class_name = entrypoint.split(":", 1)
        module = importlib.import_module(mod_name)
    else:
        # Path inside the unpacked archive: ``agent/plugin.py``.
        path = install_dir / entrypoint
        if not path.exists():
            raise ManifestError(
                f"agent entrypoint {entrypoint} not found in {install_dir}"
            )
        spec = importlib.util.spec_from_file_location(
            f"_ados_plugin_{manifest.id}", path
        )
        if spec is None or spec.loader is None:
            raise ManifestError(f"cannot load entrypoint at {path}")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        class_name = "Plugin"
    klass = getattr(module, class_name, None)
    if klass is None:
        raise ManifestError(
            f"entrypoint module has no {class_name} class"
        )
    return klass


async def _run(plugin_id: str) -> int:
    install_dir = PLUGINS_INSTALL_DIR / plugin_id
    manifest_path = install_dir / MANIFEST_FILENAME
    if not manifest_path.exists():
        log.error("plugin_manifest_missing", path=str(manifest_path))
        return 2
    try:
        manifest = PluginManifest.from_yaml_file(manifest_path)
    except ManifestError as exc:
        log.error("plugin_manifest_invalid", error=str(exc))
        return 2

    try:
        klass = _load_plugin_class(install_dir, manifest)
    except (ManifestError, ImportError) as exc:
        log.error("plugin_load_failed", plugin_id=plugin_id, error=str(exc))
        return 2

    plugin = klass()

    # The bare context is intentionally minimal so plugin lifecycle
    # hooks have a stable argument shape before the IPC bridge to the
    # supervisor is wired through.
    ctx = _BarePluginContext(plugin_id=plugin_id, version=manifest.version)

    shutdown = asyncio.Event()

    def _signal_handler(_signum: int, _frame) -> None:
        log.info("plugin_runner_signal_received", plugin_id=plugin_id)
        shutdown.set()

    signal.signal(signal.SIGTERM, _signal_handler)
    signal.signal(signal.SIGINT, _signal_handler)

    try:
        await _maybe_await(getattr(plugin, "on_install", None), ctx)
        await _maybe_await(getattr(plugin, "on_enable", None), ctx)
        await _maybe_await(getattr(plugin, "on_configure", None), ctx, {})
        await _maybe_await(getattr(plugin, "on_start", None), ctx)
        log.info("plugin_runner_ready", plugin_id=plugin_id)
        await shutdown.wait()
        await _maybe_await(getattr(plugin, "on_stop", None), ctx)
        await _maybe_await(getattr(plugin, "on_disable", None), ctx)
    except PluginError as exc:
        log.error("plugin_runtime_error", plugin_id=plugin_id, error=str(exc))
        return 1
    except Exception as exc:  # noqa: BLE001
        log.error(
            "plugin_runtime_unhandled",
            plugin_id=plugin_id,
            error=str(exc),
            error_type=type(exc).__name__,
        )
        return 1

    log.info("plugin_runner_clean_exit", plugin_id=plugin_id)
    return 0


async def _maybe_await(callable_or_none, *args) -> None:
    if callable_or_none is None:
        return
    result = callable_or_none(*args)
    if asyncio.iscoroutine(result):
        await result


class _BarePluginContext:
    """Bare placeholder context.

    Replaced by the real IPC client (UDS to supervisor, capability
    tokens, event bus, MAVLink, HAL) once the supervisor IPC server
    is reachable from this process.
    """

    def __init__(self, *, plugin_id: str, version: str) -> None:
        self.plugin_id = plugin_id
        self.plugin_version = version
        self.config: dict = {}
        self.log = get_logger(f"plugin.{plugin_id}")


@click.command()
@click.argument("plugin_id")
def main(plugin_id: str) -> None:
    configure_logging()
    code = asyncio.run(_run(plugin_id))
    sys.exit(code)


if __name__ == "__main__":
    main()
