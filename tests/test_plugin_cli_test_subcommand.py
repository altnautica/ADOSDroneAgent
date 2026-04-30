"""Coverage for the ``ados plugin test`` CLI subcommand.

The subcommand is a thin wrapper that validates the plugin manifest,
sets up environment variables, and shells out to ``pytest``. Tests
build a tiny on-disk plugin layout and invoke the command via Click's
``CliRunner``.
"""

from __future__ import annotations

import json
from pathlib import Path

from click.testing import CliRunner

from ados.cli.plugin import plugin_group


_VALID_MANIFEST = """
schema_version: 1
id: com.example.cli
version: 1.0.0
name: CLI Test
description: ""
license: MIT
risk: low
compatibility:
  ados_version: ">=0.9.0"
agent:
  entrypoint: plugin.py
  permissions:
    - id: event.publish
      required: true
  test_fixtures:
    happy: fixtures/happy.yaml
"""


def _write_plugin(root: Path, *, with_tests: bool = True) -> None:
    (root / "manifest.yaml").write_text(_VALID_MANIFEST, encoding="utf-8")
    (root / "plugin.py").write_text("# stub\n", encoding="utf-8")
    if with_tests:
        tests_dir = root / "tests"
        tests_dir.mkdir()
        (tests_dir / "test_smoke.py").write_text(
            "def test_smoke():\n    assert 1 + 1 == 2\n",
            encoding="utf-8",
        )


def test_subcommand_rejects_missing_manifest(tmp_path: Path) -> None:
    runner = CliRunner()
    result = runner.invoke(plugin_group, ["test", str(tmp_path), "--json"])
    assert result.exit_code == 2  # EXIT_MANIFEST_INVALID
    payload = json.loads(result.output.strip().splitlines()[-1])
    assert payload["ok"] is False
    assert payload["kind"] == "manifest_invalid"


def test_subcommand_rejects_missing_tests_dir(tmp_path: Path) -> None:
    _write_plugin(tmp_path, with_tests=False)
    runner = CliRunner()
    result = runner.invoke(plugin_group, ["test", str(tmp_path), "--json"])
    assert result.exit_code == 5  # EXIT_NOT_FOUND
    payload = json.loads(result.output.strip().splitlines()[-1])
    assert payload["kind"] == "plugin_not_found"


def test_subcommand_runs_pytest_and_reports_success(tmp_path: Path) -> None:
    _write_plugin(tmp_path)
    runner = CliRunner()
    result = runner.invoke(plugin_group, ["test", str(tmp_path), "--json"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output.strip().splitlines()[-1])
    assert payload["ok"] is True
    assert payload["data"]["plugin_id"] == "com.example.cli"


def test_subcommand_propagates_pytest_failure(tmp_path: Path) -> None:
    _write_plugin(tmp_path, with_tests=False)
    tests_dir = tmp_path / "tests"
    tests_dir.mkdir()
    (tests_dir / "test_fail.py").write_text(
        "def test_fail():\n    assert False\n",
        encoding="utf-8",
    )
    runner = CliRunner()
    result = runner.invoke(plugin_group, ["test", str(tmp_path), "--json"])
    assert result.exit_code == 1


def test_manifest_test_fixtures_traversal_rejected(tmp_path: Path) -> None:
    bad = """
schema_version: 1
id: com.example.bad
version: 1.0.0
name: Bad
license: MIT
compatibility:
  ados_version: ">=0.9.0"
agent:
  entrypoint: plugin.py
  test_fixtures:
    bad: ../etc/passwd
"""
    (tmp_path / "manifest.yaml").write_text(bad, encoding="utf-8")
    (tmp_path / "plugin.py").write_text("# stub\n", encoding="utf-8")
    (tmp_path / "tests").mkdir()

    runner = CliRunner()
    result = runner.invoke(plugin_group, ["test", str(tmp_path), "--json"])
    assert result.exit_code == 2
    payload = json.loads(result.output.strip().splitlines()[-1])
    assert payload["kind"] == "manifest_invalid"
