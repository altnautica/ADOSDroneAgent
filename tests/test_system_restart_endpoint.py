"""Tests for ``POST /api/v1/system/restart-supervisor``."""

from __future__ import annotations

import subprocess
from unittest.mock import patch

import pytest
from fastapi.testclient import TestClient

from ados.api.server import create_app
from tests.api_runtime_utils import build_api_runtime


@pytest.fixture
def client() -> TestClient:
    runtime = build_api_runtime()
    return TestClient(create_app(runtime))


def test_restart_supervisor_returns_ok_when_systemctl_available(
    client: TestClient,
) -> None:
    """Endpoint reports ok=True and schedules the systemctl call.

    FastAPI runs ``BackgroundTasks`` after the response body is sent
    but before the request finishes; ``TestClient.post`` therefore
    blocks until the background coroutine has executed, which lets us
    assert the systemctl call really fired.
    """
    scheduled: list[list[str]] = []

    def fake_run(cmd, *args, **kwargs):  # type: ignore[no-untyped-def]
        scheduled.append(list(cmd))
        return subprocess.CompletedProcess(
            args=cmd, returncode=0, stdout="", stderr="",
        )

    with patch(
        "ados.api.routes.system.shutil.which",
        return_value="/usr/bin/systemctl",
    ), patch(
        "ados.api.routes.system.subprocess.run", side_effect=fake_run,
    ):
        resp = client.post("/api/v1/system/restart-supervisor")

    assert resp.status_code == 200
    body = resp.json()
    assert body["ok"] is True
    assert "ados-supervisor" in body["message"]
    # The background task should have invoked systemctl.
    assert any(
        cmd == ["systemctl", "restart", "ados-supervisor"]
        for cmd in scheduled
    )


def test_restart_supervisor_reports_missing_systemctl(client: TestClient) -> None:
    """If systemctl is not on PATH, the endpoint returns ok=False."""
    with patch("ados.api.routes.system.shutil.which", return_value=None):
        resp = client.post("/api/v1/system/restart-supervisor")
    assert resp.status_code == 200
    body = resp.json()
    assert body["ok"] is False
    assert "systemctl" in body["message"]
