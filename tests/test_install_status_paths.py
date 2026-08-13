# SPDX-License-Identifier: GPL-3.0-only
# Copyright (C) 2026 Altnautica — ADOS Drone Agent
"""`ados install --status` must read the path this platform actually installs to.

The CLI carried its own `Path("/var/lib/ados/install-result.json")` literal, so on
a macOS workstation — where `ados-installer`'s `macos.rs` records the result under
`$HOME/.ados` — `--status` read a path that cannot exist and then printed that
same impossible path in its "nothing recorded" message. The resolution belongs to
`ados.core.paths`, which is platform-aware and honours the env overrides the
workstation daemons export.
"""

from __future__ import annotations

import importlib
import json
import os
from pathlib import Path

from click.testing import CliRunner


def _reload_paths(monkeypatch, *, macos: bool, home: Path):
    """Re-import `ados.core.paths` (and the CLI that binds its constants) with the
    platform + HOME this test is asserting about.

    The constants are module-level, so the platform decision is made at import
    time; monkeypatching `_IS_MACOS` after the fact would leave them stale.
    """
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("ADOS_HOME", str(home / ".ados"))
    for var in ("ADOS_INSTALL_RESULT", "ADOS_INSTALL_CHECKPOINT_DIR", "ADOS_LIB_DIR"):
        monkeypatch.delenv(var, raising=False)
    monkeypatch.setattr("platform.system", lambda: "Darwin" if macos else "Linux")

    import ados.core.paths as paths

    paths = importlib.reload(paths)
    import ados.cli.main as cli_main

    cli_main = importlib.reload(cli_main)
    return paths, cli_main


def test_install_status_reads_the_macos_install_result(monkeypatch, tmp_path) -> None:
    paths, cli_main = _reload_paths(monkeypatch, macos=True, home=tmp_path)

    assert paths.INSTALL_RESULT == tmp_path / ".ados" / "install-result.json", (
        "the macOS install result must resolve under ~/.ados, not the Linux FHS"
    )
    assert paths.INSTALL_CHECKPOINT_DIR == tmp_path / ".ados" / "install-checkpoints"
    assert paths.SETUP_COMPLETE_PATH == tmp_path / ".ados" / "setup-complete"

    paths.INSTALL_RESULT.parent.mkdir(parents=True, exist_ok=True)
    paths.INSTALL_RESULT.write_text(
        json.dumps({"status": "ok", "version": "0.99.359", "profile": "drone"}),
        encoding="utf-8",
    )
    (paths.INSTALL_CHECKPOINT_DIR).mkdir(parents=True, exist_ok=True)
    (paths.INSTALL_CHECKPOINT_DIR / "deps.done").touch()

    result = CliRunner().invoke(cli_main.cli, ["install", "--status", "--json"])
    assert result.exit_code == 0, result.output
    body = json.loads(result.output)
    assert body["result"]["status"] == "ok"
    assert body["result"]["version"] == "0.99.359"
    assert body["checkpoints"]["done"] == ["deps"]
    assert "venv" in body["checkpoints"]["missing"]


def test_install_status_names_the_platform_path_when_nothing_is_recorded(
    monkeypatch, tmp_path
) -> None:
    _paths, cli_main = _reload_paths(monkeypatch, macos=True, home=tmp_path)

    result = CliRunner().invoke(cli_main.cli, ["install", "--status"])
    assert result.exit_code == 0, result.output
    assert str(tmp_path / ".ados" / "install-result.json") in result.output
    assert "/var/lib/ados" not in result.output, (
        "the not-found message must name the path this platform reads, or an "
        "operator goes looking in a directory the installer never wrote"
    )


def test_linux_resolution_is_unchanged(monkeypatch, tmp_path) -> None:
    paths, _cli_main = _reload_paths(monkeypatch, macos=False, home=tmp_path)

    assert paths.INSTALL_RESULT == Path("/var/lib/ados/install-result.json")
    assert paths.INSTALL_CHECKPOINT_DIR == Path("/var/lib/ados/install-checkpoints")
    assert paths.SETUP_COMPLETE_PATH == Path("/var/lib/ados/setup-complete")


def test_env_overrides_win_on_either_platform(monkeypatch, tmp_path) -> None:
    monkeypatch.setenv("ADOS_LIB_DIR", str(tmp_path / "state"))
    monkeypatch.setattr("platform.system", lambda: "Linux")
    import ados.core.paths as paths

    paths = importlib.reload(paths)
    assert paths.INSTALL_RESULT == tmp_path / "state" / "install-result.json"
    assert paths.INSTALL_CHECKPOINT_DIR == tmp_path / "state" / "install-checkpoints"

    monkeypatch.setenv("ADOS_INSTALL_RESULT", str(tmp_path / "elsewhere.json"))
    paths = importlib.reload(paths)
    assert paths.INSTALL_RESULT == tmp_path / "elsewhere.json"


def test_modules_are_restored_for_the_rest_of_the_suite() -> None:
    """Reloading `ados.core.paths` mutates process-wide state.

    Every test above reloads it under a monkeypatched platform; this restores the
    real modules so a later test in the same session does not inherit a macOS
    view of the filesystem on a Linux CI runner.
    """
    for var in ("ADOS_LIB_DIR", "ADOS_INSTALL_RESULT", "ADOS_INSTALL_CHECKPOINT_DIR"):
        os.environ.pop(var, None)
    import ados.core.paths as paths

    importlib.reload(paths)
    import ados.cli.main as cli_main

    importlib.reload(cli_main)
