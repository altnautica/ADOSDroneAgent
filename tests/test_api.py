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


def test_get_logs(client):
    resp = client.get("/api/logs")
    assert resp.status_code == 200
    data = resp.json()
    assert "entries" in data
    assert "total" in data


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
