"""Plugin subprocess runner.

This is the program systemd starts inside the per-plugin
``ados-plugin-<id>.service`` unit. The supervisor passes the plugin
id and the path to a Unix-domain socket plus a capability token; the
runner connects, builds a :class:`PluginContext` bound to that
connection, imports the entry-point, and runs lifecycle hooks until
shutdown.

When the supervisor socket / token are not supplied on the command
line, the runner falls back to a bare context with a null IPC client.
That path is exercised by tests that just want the lifecycle skeleton
without a live supervisor.

Exit codes:
* 0 graceful shutdown
* 1 plugin error (lifecycle hook raised)
* 2 unable to load manifest or entry-point
* 3 SIGTERM honored
"""

from __future__ import annotations

import asyncio
import importlib
import os
import signal
import sys
from pathlib import Path

import click

from ados.core.logging import configure_logging, get_logger
from ados.core.paths import (
    PLUGIN_DATA_DIR,
    PLUGIN_RUN_DIR,
    PLUGINS_INSTALL_DIR,
)
from ados.plugins.archive import MANIFEST_FILENAME
from ados.plugins.errors import ManifestError, PluginError
from ados.plugins.ipc_client import (
    PluginContext,
    PluginIpcClient,
    _BarePluginContext,
    _NullIpcClient,
)
from ados.plugins.manifest import PluginManifest
from ados.plugins.process_sandbox import (
    SpawnedProcess,
    terminate_all,
)
from ados.plugins.process_sandbox import (
    spawn as sandbox_spawn,
)

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


async def _run(
    plugin_id: str,
    *,
    socket_path: str | None,
    capability_token: str | None,
    agent_id: str,
) -> int:
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

    ipc_client: PluginIpcClient | _NullIpcClient
    if socket_path and capability_token:
        ipc_client = PluginIpcClient(
            plugin_id=plugin_id,
            token=capability_token,
            socket_path=Path(socket_path),
        )
        try:
            await ipc_client.connect()
        except Exception as exc:  # noqa: BLE001
            log.error(
                "plugin_ipc_connect_failed",
                plugin_id=plugin_id,
                error=str(exc),
            )
            # Fall back to the bare context so the lifecycle can still
            # run (useful for plugins that do not touch host services
            # in their on_install / on_disable hooks).
            ipc_client = _NullIpcClient(plugin_id)
    else:
        ipc_client = _NullIpcClient(plugin_id)

    static_config = _read_static_config(plugin_id, agent_id)
    ctx = PluginContext(
        plugin_id=plugin_id,
        plugin_version=manifest.version,
        config=static_config,
        ipc=ipc_client,  # type: ignore[arg-type]
        agent_id=agent_id,
        data_dir=_data_dir_for(plugin_id, agent_id),
        config_dir=PLUGIN_DATA_DIR / plugin_id / "config",
        temp_dir=PLUGIN_RUN_DIR / plugin_id,
    )

    # Buffer the vendor-binary subprocesses the plugin spawned so we
    # can terminate them on plugin teardown. ProcessClient.spawn
    # returns a server-authorized payload that the runner uses to
    # exec the binary; the spawned handles live here for cleanup.
    spawned: list[SpawnedProcess] = []
    ctx_process = ctx.process
    original_spawn = ctx_process.spawn

    async def _spawn_and_track(
        basename: str,
        args: list[str] | None = None,
        env: dict[str, str] | None = None,
    ) -> SpawnedProcess:
        result = await original_spawn(basename, args=args, env=env)
        if result.get("error"):
            raise PluginError(
                f"process.spawn refused: {result.get('error')} {result.get('reason', '')}"
            )
        allowlist = _spawn_allowlist(manifest)
        proc = sandbox_spawn(
            plugin_id=plugin_id,
            install_dir=Path(result.get("install_dir", install_dir)),
            allowlist=allowlist,
            basename=basename,
            args=list(args or []),
            env=dict(env or {}),
        )
        spawned.append(proc)
        return proc

    ctx_process.spawn = _spawn_and_track  # type: ignore[method-assign]

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
    finally:
        terminate_all(spawned)
        if isinstance(ipc_client, PluginIpcClient):
            try:
                await ipc_client.close()
            except Exception:  # noqa: BLE001
                pass

    log.info("plugin_runner_clean_exit", plugin_id=plugin_id)
    return 0


async def _maybe_await(callable_or_none, *args) -> None:
    if callable_or_none is None:
        return
    result = callable_or_none(*args)
    if asyncio.iscoroutine(result):
        await result


def _data_dir_for(plugin_id: str, agent_id: str) -> Path:
    base = PLUGIN_DATA_DIR / plugin_id
    if agent_id:
        return base / "drones" / agent_id
    return base


def _read_static_config(plugin_id: str, agent_id: str) -> dict:
    """Read the manifest-supplied config dict from disk if present.

    Returns an empty dict on any error so plugin lifecycle can proceed
    without a working file system. The host populates the live kv via
    ``ctx.config.get`` / ``set``.
    """
    candidates = []
    if agent_id:
        candidates.append(
            PLUGIN_DATA_DIR / plugin_id / "config" / f"{agent_id}.yaml"
        )
    candidates.append(PLUGIN_DATA_DIR / plugin_id / "config.yaml")
    for path in candidates:
        if not path.exists():
            continue
        try:
            import yaml  # type: ignore[import-untyped]

            with open(path) as fh:
                data = yaml.safe_load(fh)
            if isinstance(data, dict):
                return data
        except Exception as exc:  # noqa: BLE001
            log.warning(
                "plugin_config_load_failed",
                plugin_id=plugin_id,
                path=str(path),
                error=str(exc),
            )
    return {}


def _spawn_allowlist(manifest: PluginManifest) -> frozenset[str]:
    """Extract the manifest's ``agent.subprocess_spawn`` allowlist.

    The manifest field is added by the v1.1 manifest schema work;
    until that lands the AgentBlock will not carry the attribute, and
    we treat that as an empty allowlist (every spawn is denied).
    """
    agent_block = manifest.agent
    if agent_block is None:
        return frozenset()
    raw = getattr(agent_block, "subprocess_spawn", None) or []
    return frozenset(str(name) for name in raw)


@click.command()
@click.argument("plugin_id")
@click.option(
    "--socket",
    "socket_path",
    default=lambda: os.environ.get("ADOS_PLUGIN_SOCKET"),
    help="UDS path to the supervisor IPC server for this plugin.",
)
@click.option(
    "--token",
    "capability_token",
    default=lambda: os.environ.get("ADOS_PLUGIN_TOKEN"),
    help="Capability token minted by the supervisor for this plugin process.",
)
@click.option(
    "--agent-id",
    "agent_id",
    default=lambda: os.environ.get("ADOS_PLUGIN_AGENT_ID", ""),
    help="cmd_drones._id of the drone this plugin instance targets (per_drone_config).",
)
def main(
    plugin_id: str,
    socket_path: str | None,
    capability_token: str | None,
    agent_id: str,
) -> None:
    configure_logging()
    code = asyncio.run(
        _run(
            plugin_id,
            socket_path=socket_path,
            capability_token=capability_token,
            agent_id=agent_id or "",
        )
    )
    sys.exit(code)


# Re-export for back-compat. Older callers import _BarePluginContext from
# :mod:`ados.plugins.runner`; the class itself moved to
# :mod:`ados.plugins.ipc_client` so the runner-side import preserved the
# v1.0 surface without duplicating the body.
__all__ = ["_BarePluginContext", "main"]
