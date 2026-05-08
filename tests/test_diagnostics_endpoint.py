"""Tests for the ``/api/v1/diagnostics`` endpoint."""

from __future__ import annotations

import subprocess
from unittest.mock import patch

import pytest
from fastapi.testclient import TestClient

from ados.api.routes import diagnostics as diag_module
from ados.api.server import create_app
from tests.api_runtime_utils import build_api_runtime


@pytest.fixture(autouse=True)
def _reset_diag_cache() -> None:
    """Reset the response cache so tests don't bleed state into each other."""
    diag_module._cache_value = None
    diag_module._cache_at = 0.0
    yield
    diag_module._cache_value = None
    diag_module._cache_at = 0.0


@pytest.fixture
def client() -> TestClient:
    runtime = build_api_runtime()
    return TestClient(create_app(runtime))


class _FakeProc:
    def __init__(self, *, stdout: str = "", stderr: str = "", returncode: int = 0) -> None:
        self.stdout = stdout
        self.stderr = stderr
        self.returncode = returncode


def test_diagnostics_returns_full_schema(client: TestClient) -> None:
    """Endpoint composes all five sections regardless of host environment."""

    def fake_run(cmd, *args, **kwargs):  # type: ignore[no-untyped-def]
        if cmd[:2] == ["journalctl", "-u"]:
            return _FakeProc(
                stdout="line one\nline two\nline three\n",
                returncode=0,
            )
        if cmd[:2] == ["ip", "-4"]:
            return _FakeProc(
                stdout=(
                    "1: lo    inet 127.0.0.1/8 scope host lo\n"
                    "2: eth0   inet 192.168.1.42/24 brd ... scope global eth0\n"
                ),
                returncode=0,
            )
        return _FakeProc(returncode=1)

    with patch.object(subprocess, "run", side_effect=fake_run):
        resp = client.get("/api/v1/diagnostics")
    assert resp.status_code == 200
    body = resp.json()

    # Top-level shape.
    for key in ("agent", "board", "system", "network", "device", "logs"):
        assert key in body, f"missing {key} in diagnostics payload"

    agent = body["agent"]
    assert agent.get("version")
    assert "uptime_seconds" in agent
    assert "process_cpu_percent" in agent
    assert "process_memory_mb" in agent

    board = body["board"]
    for k in ("name", "soc", "arch", "ram_total_mb"):
        assert k in board

    system = body["system"]
    for k in (
        "cpu_percent",
        "memory_used_mb",
        "memory_total_mb",
        "disk_used_gb",
        "disk_total_gb",
        "load_avg",
    ):
        assert k in system
    assert isinstance(system["load_avg"], list)
    assert len(system["load_avg"]) == 3

    network = body["network"]
    assert "ip" in network
    assert network.get("ip") == "192.168.1.42"
    assert "mac_eth0" in network
    assert "mac_wlan0" in network

    device = body["device"]
    assert "device_id" in device

    logs = body["logs"]
    assert "agent" in logs
    assert isinstance(logs["agent"], list)
    assert "line three" in logs["agent"]


def test_diagnostics_response_is_cached_for_one_second(client: TestClient) -> None:
    """Repeated polls within the cache TTL hit the cache, not the helpers."""

    call_count = {"runs": 0}

    def fake_run(cmd, *args, **kwargs):  # type: ignore[no-untyped-def]
        call_count["runs"] += 1
        if cmd[:2] == ["journalctl", "-u"]:
            return _FakeProc(stdout="cached line\n", returncode=0)
        if cmd[:2] == ["ip", "-4"]:
            return _FakeProc(
                stdout="2: eth0   inet 10.0.0.1/24 scope global eth0\n",
                returncode=0,
            )
        return _FakeProc(returncode=1)

    with patch.object(subprocess, "run", side_effect=fake_run):
        first = client.get("/api/v1/diagnostics")
        runs_after_first = call_count["runs"]
        second = client.get("/api/v1/diagnostics")

    assert first.status_code == 200
    assert second.status_code == 200
    # The second request must NOT have triggered fresh subprocess calls.
    assert call_count["runs"] == runs_after_first


def test_diagnostics_handles_journalctl_error(client: TestClient) -> None:
    """A failing journalctl is captured as a single error line, not a 500."""

    def fake_run(cmd, *args, **kwargs):  # type: ignore[no-untyped-def]
        if cmd[:2] == ["journalctl", "-u"]:
            return _FakeProc(
                stderr="No journal files were found.\n",
                returncode=1,
            )
        return _FakeProc(returncode=1)

    with patch.object(subprocess, "run", side_effect=fake_run):
        resp = client.get("/api/v1/diagnostics")
    assert resp.status_code == 200
    logs = resp.json()["logs"]["agent"]
    assert logs
    assert any("journalctl error" in line for line in logs)
