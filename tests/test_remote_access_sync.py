"""The tunnel-unit restart acts only on a tunnel unit and reports its outcome."""

from __future__ import annotations

import subprocess

import ados.core.remote_access_sync as remote_access_sync
from ados.core.remote_access_sync import restart_tunnel_unit


class _Recorder:
    def __init__(self, returncode: int = 0, stderr: bytes = b"") -> None:
        self.calls: list[list[str]] = []
        self.returncode = returncode
        self.stderr = stderr

    def __call__(self, argv, **_kwargs):
        self.calls.append(list(argv))
        outcome = self

        class _Done:
            returncode = outcome.returncode
            stderr = outcome.stderr

        return _Done()


def test_restart_tunnel_unit_validates_name(monkeypatch) -> None:
    rec = _Recorder()
    monkeypatch.setattr(remote_access_sync.subprocess, "run", rec)

    for unit in ("sshd", "ados-control", "cloudflared;reboot", "cloudflared x", ""):
        result = restart_tunnel_unit(unit)
        assert result == {"unit": unit, "ok": False, "error": "unit name refused"}
    assert rec.calls == []


def test_restart_tunnel_unit_try_restarts_without_blocking(monkeypatch) -> None:
    rec = _Recorder()
    monkeypatch.setattr(remote_access_sync.subprocess, "run", rec)

    result = restart_tunnel_unit("cloudflared.service")

    assert result == {"unit": "cloudflared.service", "ok": True, "error": None}
    assert rec.calls == [["systemctl", "--no-block", "try-restart", "cloudflared.service"]]


def test_restart_tunnel_unit_reports_a_failed_restart(monkeypatch) -> None:
    monkeypatch.setattr(
        remote_access_sync.subprocess, "run", _Recorder(returncode=5, stderr=b"not loaded")
    )
    assert restart_tunnel_unit("cloudflared") == {
        "unit": "cloudflared",
        "ok": False,
        "error": "not loaded",
    }

    def _boom(*_args, **_kwargs):
        raise subprocess.TimeoutExpired("systemctl", 10)

    monkeypatch.setattr(remote_access_sync.subprocess, "run", _boom)
    result = restart_tunnel_unit("cloudflared")
    assert result["ok"] is False
    assert result["error"]
