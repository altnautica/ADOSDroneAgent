"""Import a plugin's agent-half class from its unpacked install directory."""

from __future__ import annotations

import importlib
import sys
from pathlib import Path

from ados.plugins.errors import ManifestError
from ados.plugins.manifest import PluginManifest


def _ensure_plugin_src_on_path(install_dir: Path) -> None:
    """Make a third-party plugin's unpacked source and wheels importable.

    A plugin archive unpacks its agent source under the install dir, so the
    runner subprocess must put that source root on ``sys.path`` before
    importing the entry point. Built-in plugins live in the agent's own
    package and need none of this, but a ``module:Class`` (or a file-path)
    entry point whose modules ship inside the archive only resolves once their
    package root is importable.

    Every present root among ``agent/src``, ``agent/py``, and a flat ``agent``
    is added (a plugin normally ships one layout, but a mixed one — a package
    under ``agent/src`` plus loose modules directly under ``agent`` — resolves
    too). Roots are APPENDED, never prepended, so a plugin can never shadow the
    agent's own packages or the standard library; the unique top-level packages
    shipped inside the archive still resolve. Each runner runs a single plugin
    in its own subprocess, so the mutation is local.

    ``agent/site`` is the per-plugin dependency dir the install path fills
    from the archive's vendored ``agent/wheel/*.whl``. It is appended on the
    same terms as the source roots: a plugin's dependency can never shadow the
    agent's own copy of a package, which matters because the agent and a
    plugin may want different versions of the same library and the flight path
    must keep the one it was tested against.
    """
    for rel in ("agent/src", "agent/py", "agent", "agent/site"):
        root = install_dir / rel
        if root.is_dir():
            entry = str(root)
            if entry not in sys.path:
                sys.path.append(entry)


def _load_plugin_class(install_dir: Path, manifest: PluginManifest):
    if manifest.agent is None:
        raise ManifestError("plugin has no agent half; nothing to run")
    _ensure_plugin_src_on_path(install_dir)
    entrypoint = manifest.agent.entrypoint
    if ":" in entrypoint:
        # ``module:Class`` style: a built-in plugin in the agent package, or a
        # third-party plugin whose source root is now on ``sys.path``.
        mod_name, class_name = entrypoint.split(":", 1)
        module = importlib.import_module(mod_name)
    else:
        # Path inside the unpacked archive: ``agent/plugin.py``.
        path = install_dir / entrypoint
        if not path.exists():
            raise ManifestError(
                f"agent entrypoint {entrypoint} not found in {install_dir}"
            )
        # The entry file's own directory must be importable so its sibling
        # modules resolve regardless of which source root matched above.
        parent = str(path.parent)
        if parent not in sys.path:
            sys.path.append(parent)
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
