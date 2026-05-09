"""Tests for the systemctl-stop kill-fallback in the CLI uninstall path.

The previous code passed timeout=30 to `systemctl stop` and let
`subprocess.TimeoutExpired` propagate, crashing the uninstall mid-
transaction so symlinks and directories never got cleaned up. The
helper now tolerates timeouts and other OS errors so the cleanup
phase always runs.
"""

from __future__ import annotations

import subprocess
from unittest.mock import patch

from ados.cli import main as cli_main


def _make_completed_proc(rc: int = 0) -> subprocess.CompletedProcess:
    return subprocess.CompletedProcess(args=[], returncode=rc, stdout=b"", stderr=b"")


def test_stop_service_first_call_succeeds_no_kill_needed():
    """Happy path: first systemctl stop returns 0, no escalation."""
    with patch.object(cli_main.shutil, "which", return_value="/bin/systemctl"), \
         patch.object(cli_main.subprocess, "run") as mock_run:
        mock_run.return_value = _make_completed_proc(0)
        cli_main._stop_service_with_kill_fallback("ados-fake.service")

    # Exactly one call: the graceful stop. No kill, no second stop.
    assert mock_run.call_count == 1
    cmd = mock_run.call_args.args[0]
    assert cmd == ["systemctl", "stop", "ados-fake.service"]


def test_stop_service_timeout_escalates_to_sigkill():
    """Graceful stop times out -> SIGKILL -> second short stop. The
    helper must not raise even if every escalation fails."""
    call_log: list[list[str]] = []

    def _run_side_effect(cmd, **_kw):
        call_log.append(list(cmd))
        if cmd[:2] == ["systemctl", "stop"] and len(call_log) == 1:
            # First stop times out.
            raise subprocess.TimeoutExpired(cmd=cmd, timeout=60)
        return _make_completed_proc(0)

    with patch.object(cli_main.shutil, "which", return_value="/bin/systemctl"), \
         patch.object(cli_main.subprocess, "run", side_effect=_run_side_effect):
        cli_main._stop_service_with_kill_fallback("ados-stuck.service")

    assert len(call_log) == 3
    assert call_log[0] == ["systemctl", "stop", "ados-stuck.service"]
    assert call_log[1][:3] == ["systemctl", "kill", "-s"]
    assert "SIGKILL" in call_log[1]
    assert call_log[1][-1] == "ados-stuck.service"
    assert call_log[2] == ["systemctl", "stop", "ados-stuck.service"]


def test_stop_service_oserror_does_not_propagate():
    """OSError on systemctl invocation must not crash the uninstall."""
    with patch.object(cli_main.shutil, "which", return_value="/bin/systemctl"), \
         patch.object(
             cli_main.subprocess,
             "run",
             side_effect=OSError("permission denied"),
         ):
        # Must not raise.
        cli_main._stop_service_with_kill_fallback("ados-broken.service")


def test_stop_service_skips_when_systemctl_missing():
    """No systemctl on PATH (e.g., macOS dev box): helper is a no-op."""
    with patch.object(cli_main.shutil, "which", return_value=None), \
         patch.object(cli_main.subprocess, "run") as mock_run:
        cli_main._stop_service_with_kill_fallback("ados-anything.service")
    assert mock_run.call_count == 0


def test_stop_service_kill_failure_still_attempts_post_kill_stop():
    """Even when SIGKILL itself fails, the helper still tries the
    second stop so systemd's tracking can clear."""
    call_log: list[list[str]] = []

    def _run_side_effect(cmd, **_kw):
        call_log.append(list(cmd))
        if cmd[:2] == ["systemctl", "stop"] and len(call_log) == 1:
            raise subprocess.TimeoutExpired(cmd=cmd, timeout=60)
        if cmd[:2] == ["systemctl", "kill"]:
            raise OSError("no such process")
        return _make_completed_proc(0)

    with patch.object(cli_main.shutil, "which", return_value="/bin/systemctl"), \
         patch.object(cli_main.subprocess, "run", side_effect=_run_side_effect):
        cli_main._stop_service_with_kill_fallback("ados-zombie.service")

    # Three attempts total: stop / kill (failed) / stop. None raised.
    assert len(call_log) == 3
    assert call_log[2][:2] == ["systemctl", "stop"]
