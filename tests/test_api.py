"""Tests for FastAPI REST API routes."""

from __future__ import annotations

import pytest
from fastapi.testclient import TestClient

from ados.api.server import create_app
from tests.api_runtime_utils import build_api_runtime


@pytest.fixture
def agent_app():
    """Create an API runtime double for testing."""
    return build_api_runtime()


@pytest.fixture
def client(agent_app):
    """FastAPI test client."""
    fastapi_app = create_app(agent_app)
    return TestClient(fastapi_app)


def test_health_check(client):
    resp = client.get("/healthz")
    assert resp.status_code == 200
    data = resp.json()
    assert data["status"] == "ok"
    assert "version" in data


def test_get_status(client):
    resp = client.get("/api/status")
    assert resp.status_code == 200
    data = resp.json()
    assert "version" in data
    assert "uptime_seconds" in data
    assert "fc_connected" in data


def test_get_setup_status(client):
    resp = client.get("/api/v1/setup/status")
    assert resp.status_code == 200
    data = resp.json()
    assert data["device_id"]
    assert "steps" in data
    assert "access_urls" in data
    assert "mavlink" in data
    assert "video" in data
    # A live agent always reports "configured". Auto-detect commits a
    # profile at install, the operator can override via the webapp at
    # any time, and there is no intermediate "needs review" state.
    assert data["setup_state"] == "configured"
    assert data["profile_source"] in (
        "detected",
        "tiebreaker",
        "override",
        "default",
        "user",
    )
    assert data["profile_suggestion"]["detected"] in ("drone", "ground_station")
    assert data["profile_suggestion"]["source"] in (
        "detected",
        "tiebreaker",
        "override",
        "default",
    )


def test_get_telemetry(client):
    resp = client.get("/api/telemetry")
    assert resp.status_code == 200
    data = resp.json()
    assert "position" in data
    assert "attitude" in data
    assert "battery" in data


def test_get_services(client):
    resp = client.get("/api/services")
    assert resp.status_code == 200
    data = resp.json()
    assert "services" in data


def test_get_services_includes_per_service_memory(agent_app, monkeypatch):
    """Every service entry carries a numeric ``memory_mb`` and the process
    block carries ``process.memory_mb``.

    The GCS Memory panel's per-service breakdown reads ``memory_mb`` off each
    entry; if the route ever stops attaching it the panel collapses to its
    "needs agent accounting" empty state. The plain shape check in
    ``test_get_services`` did not catch that, so this guard pins the field.
    """
    from ados.core import systemd_memory as sm
    from ados.core.service_tracker import ServiceState

    # Force one tracked, running service so the route does not fall through
    # to the systemd inventory (which would be empty in the test env). The
    # name resolves to a real unit via ``unit_for_service``.
    agent_app.services.set_state("health-monitor", ServiceState.RUNNING)

    # Pin a non-zero cgroup-accounted value for that unit so the per-entry
    # assertion exercises a real number, not an all-zero vacuous pass.
    monkeypatch.setattr(
        sm, "_pss_map", lambda: {"ados-health.service": 42.5}
    )

    fastapi_app = create_app(agent_app)
    resp = TestClient(fastapi_app).get("/api/services")
    assert resp.status_code == 200
    data = resp.json()

    assert data["services"], "expected at least one service entry"
    saw_nonzero = False
    for svc in data["services"]:
        assert "memory_mb" in svc, f"service entry missing memory_mb: {svc}"
        assert isinstance(svc["memory_mb"], (int, float))
        if svc["memory_mb"]:
            saw_nonzero = True
    assert saw_nonzero, "per-service memory_mb never populated (regression)"

    assert isinstance(data["process"]["memory_mb"], (int, float))


def test_get_params_empty(client):
    resp = client.get("/api/params")
    assert resp.status_code == 200
    data = resp.json()
    assert data["cached"] == 0


def test_get_param_not_found(client):
    resp = client.get("/api/params/NONEXISTENT")
    assert resp.status_code == 404


def test_get_config(client):
    resp = client.get("/api/config")
    assert resp.status_code == 200
    data = resp.json()
    assert "agent" in data
    assert "mavlink" in data


def test_update_config(client):
    resp = client.put("/api/config", json={"key": "agent.name", "value": "new-drone"})
    assert resp.status_code == 200
    data = resp.json()
    assert data["status"] == "ok"


def test_get_logs_degrades_when_store_unreachable(client):
    """With no logging store socket present (the test environment), the legacy
    /api/logs endpoint degrades to an empty list with a warning rather than a
    500: losing history degrades debugging, not flight."""
    resp = client.get("/api/logs")
    assert resp.status_code == 200
    data = resp.json()
    assert data["entries"] == []
    assert data["total"] == 0
    assert "warning" in data


def test_legacy_entry_timestamp_is_iso_string():
    """The legacy entry mapping must emit an ISO-8601 string timestamp, not a
    float. A numeric timestamp breaks consumers that slice/parse the field as a
    string (the live dashboard log view does exactly that)."""
    from datetime import datetime

    from ados.api.routes.logs import _legacy_entry

    entry = _legacy_entry(
        {
            "id": 7,
            "ts_us": 1_700_000_000_000_000,
            "source": "python-agent",
            "level": "info",
            "target": "ados.test",
            "msg": "hello",
        }
    )
    assert isinstance(entry["timestamp"], str)
    # Round-trips through the ISO parser without raising.
    datetime.fromisoformat(entry["timestamp"])
    # The legacy consumer expects an upper-case level and the logger name.
    assert entry["level"] == "INFO"
    assert entry["logger"] == "ados.test"
    assert entry["message"] == "hello"


def test_list_commands(client):
    resp = client.get("/api/commands")
    assert resp.status_code == 200
    data = resp.json()
    assert "arm" in data["commands"]
    assert "takeoff" in data["commands"]


def test_command_no_fc(client):
    """Commands should fail with 503 if FC not connected."""
    resp = client.post("/api/command", json={"cmd": "arm"})
    assert resp.status_code == 503
