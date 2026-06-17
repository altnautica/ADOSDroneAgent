"""The runner must make a third-party plugin's unpacked source importable.

A plugin archive unpacks its agent source under the install dir without a
pip/wheel install. A ``module:Class`` entry point (and a file-path entry point
whose own modules ship inside the archive) therefore only resolves once the
source root is on ``sys.path``. Built-in plugins live in the agent package and
never need this; these tests cover the loose-source case for a multi-module
plugin, including the cross-module import inside the package.
"""

from __future__ import annotations

import sys
from pathlib import Path

import ados.plugins.runner as runner
from ados.plugins.manifest import PluginManifest

_MANIFEST = """\
schema_version: 1
id: com.example.loose
version: 1.0.0
name: Loose Source
license: GPL-3.0-or-later
risk: medium
compatibility:
  ados_version: ">=0.9.0"
agent:
  entrypoint: {entrypoint}
"""


def _seed_src_layout(install_dir: Path) -> None:
    """Write a two-module package under ``agent/src`` where the entry-point
    module imports a sibling module (the cross-module import must resolve)."""
    pkg = install_dir / "agent" / "src" / "sample_pkg"
    pkg.mkdir(parents=True, exist_ok=True)
    (pkg / "__init__.py").write_text("", encoding="utf-8")
    (pkg / "core.py").write_text("MARKER = 'core-loaded'\n", encoding="utf-8")
    (pkg / "plugin.py").write_text(
        "from sample_pkg.core import MARKER\n"
        "class SamplePlugin:\n"
        "    marker = MARKER\n",
        encoding="utf-8",
    )


def _clean_import_state(snapshot: list[str]) -> None:
    sys.path[:] = snapshot
    for name in list(sys.modules):
        if name == "sample_pkg" or name.startswith("sample_pkg."):
            del sys.modules[name]


def test_loose_source_module_class_entrypoint_resolves(tmp_path) -> None:
    install_dir = tmp_path / "com.example.loose"
    _seed_src_layout(install_dir)
    manifest = PluginManifest.from_yaml_text(
        _MANIFEST.format(entrypoint="sample_pkg.plugin:SamplePlugin")
    )

    snapshot = list(sys.path)
    try:
        klass = runner._load_plugin_class(install_dir, manifest)
        # The class loaded AND its cross-package import resolved.
        assert klass.__name__ == "SamplePlugin"
        assert klass.marker == "core-loaded"
        # The source root is on the path, and is APPENDED (never prepended) so
        # it cannot shadow the agent's own packages or the standard library:
        # sys.path[0] is unchanged.
        assert str(install_dir / "agent" / "src") in sys.path
        assert sys.path[0] == snapshot[0]
    finally:
        _clean_import_state(snapshot)


def test_mixed_layout_resolves_package_and_loose_module(tmp_path) -> None:
    # A plugin that ships BOTH a package under agent/src AND a loose top-level
    # module directly under agent/ must resolve both (every present root is
    # added, not just the first).
    install_dir = tmp_path / "com.example.loose"
    _seed_src_layout(install_dir)
    (install_dir / "agent" / "extra.py").write_text(
        "EXTRA = 'extra-loaded'\n", encoding="utf-8"
    )
    (install_dir / "agent" / "src" / "sample_pkg" / "plugin.py").write_text(
        "from sample_pkg.core import MARKER\n"
        "from extra import EXTRA\n"
        "class SamplePlugin:\n"
        "    marker = MARKER\n"
        "    extra = EXTRA\n",
        encoding="utf-8",
    )
    manifest = PluginManifest.from_yaml_text(
        _MANIFEST.format(entrypoint="sample_pkg.plugin:SamplePlugin")
    )
    snapshot = list(sys.path)
    try:
        klass = runner._load_plugin_class(install_dir, manifest)
        assert klass.marker == "core-loaded"  # package under agent/src
        assert klass.extra == "extra-loaded"  # loose module directly under agent/
    finally:
        for name in list(sys.modules):
            if name == "extra" or name == "sample_pkg" or name.startswith("sample_pkg."):
                del sys.modules[name]
        sys.path[:] = snapshot


def test_loose_source_path_is_idempotent(tmp_path) -> None:
    install_dir = tmp_path / "com.example.loose"
    _seed_src_layout(install_dir)
    snapshot = list(sys.path)
    try:
        runner._ensure_plugin_src_on_path(install_dir)
        runner._ensure_plugin_src_on_path(install_dir)
        entry = str(install_dir / "agent" / "src")
        # Appended exactly once even across repeated calls.
        assert sys.path.count(entry) == 1
    finally:
        sys.path[:] = snapshot
