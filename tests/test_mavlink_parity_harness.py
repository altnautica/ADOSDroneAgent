"""Run the MAVLink parity harness in demo mode and assert it goes green.

Skipped when the Rust router binary has not been built (e.g. a Python-only CI
job): the harness needs ``ados-mavlink-router`` to compare against. Build it with
``cargo build --manifest-path crates/Cargo.toml -p ados-mavlink-router`` to run
this locally.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

import pytest

_REPO_ROOT = Path(__file__).resolve().parents[1]
_HARNESS = _REPO_ROOT / "tools" / "mavlink-parity" / "parity.py"
_RUST_BIN_CANDIDATES = [
    _REPO_ROOT / "crates" / "target" / "debug" / "ados-mavlink-router",
    _REPO_ROOT / "crates" / "target" / "release" / "ados-mavlink-router",
]


def _rust_bin() -> Path | None:
    for cand in _RUST_BIN_CANDIDATES:
        if cand.exists():
            return cand
    return None


def test_demo_mode_parity_is_green(tmp_path):
    if _rust_bin() is None:
        pytest.skip("ados-mavlink-router not built; run cargo build to enable")
    report_path = tmp_path / "parity.json"
    # The unix sockets live under the workdir; macOS caps a socket path at ~104
    # chars, so a deep pytest tmp dir would overflow it. Use a short /tmp path.
    workdir = Path(f"/tmp/ados-parity-{os.getpid()}")
    shutil.rmtree(workdir, ignore_errors=True)
    try:
        result = subprocess.run(
            [
                sys.executable,
                str(_HARNESS),
                "--source",
                "demo",
                "--duration",
                "4",
                "--warmup",
                "2",
                "--workdir",
                str(workdir),
                "--json",
                str(report_path),
            ],
            capture_output=True,
            text=True,
            timeout=80,
        )
    finally:
        shutil.rmtree(workdir, ignore_errors=True)
    assert report_path.exists(), f"no report written.\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}"
    report = json.loads(report_path.read_text())
    assert report.get("ok"), f"parity not green: {report.get('summary')}\n{result.stdout}"
    summary = report["summary"]
    assert summary["fail"] == 0, f"failed checks: {report['checks']}"
    # The demo-mode comparison runs the state, fan-out, and proxy checks.
    assert summary["pass"] >= 8, f"too few checks passed: {summary}"
    assert result.returncode == 0, result.stdout
