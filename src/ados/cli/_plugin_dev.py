"""``ados plugin`` developer checks: lint and test."""

from __future__ import annotations

import json
import sys
from pathlib import Path

import click

from ados.cli._plugin_common import (
    EXIT_GENERIC,
    EXIT_MANIFEST_INVALID,
    EXIT_NOT_FOUND,
    EXIT_OK,
    _emit_err,
    _emit_ok,
    plugin_group,
)
from ados.plugins.errors import ArchiveError, ManifestError


@plugin_group.command(
    "lint", help="Run static analysis on a .adosplug archive before submission."
)
@click.argument("archive_path", type=click.Path(exists=True, dir_okay=False))
@click.option("--json", "as_json", is_flag=True)
def lint(archive_path: str, as_json: bool) -> None:
    from ados.plugins.lint import format_report, lint_archive

    try:
        report = lint_archive(archive_path)
    except (ArchiveError, ManifestError) as exc:
        code = (
            EXIT_MANIFEST_INVALID
            if isinstance(exc, ManifestError)
            else EXIT_GENERIC
        )
        _emit_err(as_json, code, str(exc))
        sys.exit(code)

    if as_json:
        _emit_ok(as_json, report.to_dict())
    else:
        click.echo(format_report(report))

    sys.exit(EXIT_OK if report.passed else EXIT_GENERIC)


@plugin_group.command(
    "test",
    help="Run a plugin's pytest suite under the SDK test harness.",
)
@click.argument(
    "plugin_dir",
    type=click.Path(exists=True, file_okay=False, dir_okay=True),
)
@click.option(
    "--tests-dir",
    "tests_dir",
    default="tests",
    show_default=True,
    help="Subdirectory under plugin_dir containing pytest tests.",
)
@click.option(
    "-k",
    "expression",
    default=None,
    help="pytest -k expression passed through to the runner.",
)
@click.option("--json", "as_json", is_flag=True)
def test_plugin(
    plugin_dir: str,
    tests_dir: str,
    expression: str | None,
    as_json: bool,
) -> None:
    """Drive pytest against the plugin's tests with the harness available
    via a conftest fixture the plugin author registers themselves. The
    subcommand validates the plugin manifest, then shells out to
    ``pytest`` so the runner stays the canonical one author tests
    were written against.
    """
    from ados.plugins.manifest import PluginManifest

    plugin_root = Path(plugin_dir)
    manifest_path = plugin_root / "manifest.yaml"
    if not manifest_path.is_file():
        _emit_err(
            as_json,
            EXIT_MANIFEST_INVALID,
            f"manifest.yaml not found at {manifest_path}",
        )
        sys.exit(EXIT_MANIFEST_INVALID)

    try:
        manifest = PluginManifest.from_yaml_file(manifest_path)
    except ManifestError as exc:
        _emit_err(as_json, EXIT_MANIFEST_INVALID, str(exc))
        sys.exit(EXIT_MANIFEST_INVALID)

    tests_path = plugin_root / tests_dir
    if not tests_path.is_dir():
        _emit_err(
            as_json,
            EXIT_NOT_FOUND,
            f"tests directory {tests_path} not found",
        )
        sys.exit(EXIT_NOT_FOUND)

    import os
    import subprocess

    env = os.environ.copy()
    env["ADOS_PLUGIN_ID"] = manifest.id
    env["ADOS_PLUGIN_VERSION"] = manifest.version
    env["ADOS_PLUGIN_ROOT"] = str(plugin_root.resolve())
    if manifest.agent and manifest.agent.test_fixtures:
        env["ADOS_PLUGIN_TEST_FIXTURES"] = json.dumps(
            manifest.agent.test_fixtures
        )

    cmd = [sys.executable, "-m", "pytest", str(tests_path)]
    if expression:
        cmd.extend(["-k", expression])
    rc = subprocess.call(cmd, env=env)
    if rc == 0:
        _emit_ok(as_json, {"plugin_id": manifest.id})
        sys.exit(EXIT_OK)
    _emit_err(as_json, EXIT_GENERIC, f"pytest exited with {rc}")
    sys.exit(EXIT_GENERIC)
