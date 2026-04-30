"""Static-analysis tests for ``.adosplug`` archives."""

from __future__ import annotations

import io
import zipfile
from pathlib import Path

import pytest

from ados.plugins.archive import MANIFEST_FILENAME
from ados.plugins.lint import (
    SEVERITY_ERROR,
    SEVERITY_INFO,
    SEVERITY_WARN,
    format_report,
    lint_archive,
)


GOOD_MANIFEST = """\
schema_version: 1
id: com.example.basic
version: 0.1.0
name: Basic
license: GPL-3.0-or-later
risk: low
compatibility:
  ados_version: ">=0.9.0,<1.0.0"
agent:
  entrypoint: agent/plugin.py
  isolation: subprocess
  permissions:
    - event.publish
"""


def _build_archive(tmp_path: Path, files: dict[str, str]) -> Path:
    out = tmp_path / "test.adosplug"
    manifest_yaml = files.get(MANIFEST_FILENAME, GOOD_MANIFEST)
    with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as zf:
        zf.writestr(MANIFEST_FILENAME, manifest_yaml)
        for path, content in files.items():
            if path == MANIFEST_FILENAME:
                continue
            zf.writestr(path, content)
    return out


def test_clean_archive_has_only_info_findings(tmp_path: Path) -> None:
    archive = _build_archive(
        tmp_path,
        {"agent/plugin.py": "def main():\n    return 'ok'\n"},
    )
    report = lint_archive(archive)
    assert report.passed
    assert report.score >= 90
    assert all(f.severity in (SEVERITY_INFO, SEVERITY_WARN) for f in report.findings)


def test_eval_call_flagged_as_error(tmp_path: Path) -> None:
    archive = _build_archive(
        tmp_path,
        {"agent/plugin.py": "def run(s):\n    return eval(s)\n"},
    )
    report = lint_archive(archive)
    error_rules = {f.rule_id for f in report.by_severity(SEVERITY_ERROR)}
    assert "PY003-eval" in error_rules
    assert not report.passed


def test_subprocess_shell_true_flagged(tmp_path: Path) -> None:
    archive = _build_archive(
        tmp_path,
        {
            "agent/plugin.py": (
                "import subprocess\n"
                "def run():\n"
                "    subprocess.Popen('ls', shell=True)\n"
            )
        },
    )
    report = lint_archive(archive)
    error_rules = {f.rule_id for f in report.by_severity(SEVERITY_ERROR)}
    assert "PY005-subprocess-shell" in error_rules


def test_network_import_warns_when_permission_missing(tmp_path: Path) -> None:
    archive = _build_archive(
        tmp_path,
        {"agent/plugin.py": "import requests\n\ndef get():\n    requests.get('https://x')\n"},
    )
    report = lint_archive(archive)
    warn_rules = {f.rule_id for f in report.by_severity(SEVERITY_WARN)}
    assert "PY020-requests" in warn_rules


def test_network_import_silent_when_permission_present(tmp_path: Path) -> None:
    manifest = """\
schema_version: 1
id: com.example.basic
version: 0.1.0
name: Basic
license: GPL-3.0-or-later
risk: medium
compatibility:
  ados_version: ">=0.9.0,<1.0.0"
agent:
  entrypoint: agent/plugin.py
  isolation: subprocess
  permissions:
    - event.publish
    - network.outbound
"""
    archive = _build_archive(
        tmp_path,
        {
            MANIFEST_FILENAME: manifest,
            "agent/plugin.py": "import requests\n",
        },
    )
    report = lint_archive(archive)
    rules = {f.rule_id for f in report.findings}
    assert "PY020-requests" not in rules


def test_gcs_eval_in_bundle_warns(tmp_path: Path) -> None:
    manifest = """\
schema_version: 1
id: com.example.gcs
version: 0.1.0
name: GcsExample
license: GPL-3.0-or-later
risk: low
compatibility:
  ados_version: ">=0.9.0,<1.0.0"
gcs:
  entrypoint: gcs/bundle.js
  isolation: iframe
  permissions:
    - ui.slot.fc-tab
"""
    archive = _build_archive(
        tmp_path,
        {
            MANIFEST_FILENAME: manifest,
            "gcs/bundle.js": "function run(s){ return eval(s); }\n",
        },
    )
    report = lint_archive(archive)
    rules = {f.rule_id for f in report.by_severity(SEVERITY_WARN)}
    assert "GCS004-eval" in rules


def test_unsigned_archive_flagged(tmp_path: Path) -> None:
    archive = _build_archive(
        tmp_path,
        {"agent/plugin.py": "x = 1\n"},
    )
    report = lint_archive(archive)
    rules = {f.rule_id for f in report.findings}
    assert "SIG001-unsigned" in rules


def test_format_report_handles_no_findings(tmp_path: Path) -> None:
    archive = _build_archive(
        tmp_path,
        {"agent/plugin.py": "x = 1\n"},
    )
    report = lint_archive(archive)
    text = format_report(report)
    assert "plugin com.example.basic 0.1.0" in text
    assert "verdict:" in text


def test_format_report_renders_findings(tmp_path: Path) -> None:
    archive = _build_archive(
        tmp_path,
        {"agent/plugin.py": "y = eval('1+1')\n"},
    )
    report = lint_archive(archive)
    text = format_report(report)
    assert "[error]" in text
    assert "PY003-eval" in text


def test_high_risk_capability_logged(tmp_path: Path) -> None:
    manifest = """\
schema_version: 1
id: com.example.cmd
version: 0.1.0
name: CmdExample
license: GPL-3.0-or-later
risk: critical
compatibility:
  ados_version: ">=0.9.0,<1.0.0"
agent:
  entrypoint: agent/plugin.py
  isolation: subprocess
  permissions:
    - vehicle.command
"""
    archive = _build_archive(
        tmp_path,
        {
            MANIFEST_FILENAME: manifest,
            "agent/plugin.py": "x = 1\n",
        },
    )
    report = lint_archive(archive)
    rules = {f.rule_id for f in report.findings}
    assert "PERM001-high-risk" in rules
