"""The plugin runner must refuse to load a rust-runtime plugin.

A rust plugin runs as its own binary (systemd execs it directly); the Python
runner has no entry point to import, so ``_run`` returns a non-zero exit code
before reaching the import path.
"""

from __future__ import annotations

import asyncio
from pathlib import Path

import ados.plugins.runner as runner
from ados.plugins.archive import MANIFEST_FILENAME

_RUST_MANIFEST = """\
schema_version: 1
id: com.example.rustplug
version: 1.0.0
name: Rust Plug
license: GPL-3.0-or-later
risk: medium
compatibility:
  ados_version: ">=0.9.0"
agent:
  entrypoint: agent/bin/com.example.rustplug
  runtime: rust
"""

_PY_MANIFEST = """\
schema_version: 1
id: com.example.pyplug
version: 1.0.0
name: Py Plug
license: GPL-3.0-or-later
risk: medium
compatibility:
  ados_version: ">=0.9.0"
agent:
  entrypoint: agent/plugin.py
"""


def _seed_plugin(install_root: Path, plugin_id: str, manifest_text: str) -> None:
    plugin_dir = install_root / plugin_id
    plugin_dir.mkdir(parents=True, exist_ok=True)
    (plugin_dir / MANIFEST_FILENAME).write_text(manifest_text, encoding="utf-8")


def test_runner_refuses_rust_plugin(tmp_path, monkeypatch) -> None:
    monkeypatch.setattr(runner, "PLUGINS_INSTALL_DIR", tmp_path)
    _seed_plugin(tmp_path, "com.example.rustplug", _RUST_MANIFEST)

    code = asyncio.run(
        runner._run(
            "com.example.rustplug",
            socket_path=None,
            capability_token=None,
            agent_id="",
        )
    )
    # Non-zero: the rust plugin must never load in the python runner.
    assert code != 0


def test_runner_loads_python_plugin_manifest(tmp_path, monkeypatch) -> None:
    # A python-runtime plugin gets past the runtime guard and reaches the
    # entry-point load step; with no entry-point file it fails at load (exit 2),
    # which proves the guard did NOT short-circuit a python plugin.
    monkeypatch.setattr(runner, "PLUGINS_INSTALL_DIR", tmp_path)
    _seed_plugin(tmp_path, "com.example.pyplug", _PY_MANIFEST)

    code = asyncio.run(
        runner._run(
            "com.example.pyplug",
            socket_path=None,
            capability_token=None,
            agent_id="",
        )
    )
    # The entry-point file is absent, so load fails with the load exit code (2),
    # not the runtime-mismatch path. Either way it is the load failure, which
    # confirms the guard passed a python plugin through.
    assert code == 2
