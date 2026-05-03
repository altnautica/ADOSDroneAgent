"""Tests for OTA API routes."""

from __future__ import annotations

import time
from unittest.mock import AsyncMock, MagicMock

import pytest
from fastapi.testclient import TestClient

from ados.api.server import create_app
from ados.core.config import ADOSConfig
from ados.core.health import HealthMonitor
from ados.core.service_tracker import ServiceTracker
from ados.services.mavlink.state import VehicleState
from ados.services.ota.manifest import UpdateManifest


def _make_manifest() -> UpdateManifest:
    return UpdateManifest(
        version="0.2.0",
        channel="stable",
        published_at="2026-03-08T00:00:00Z",
        download_url="https://updates.altnautica.com/stable/ados-0.2.0.bin",
        file_size=1024,
        sha256="a" * 64,
        changelog="Bug fixes.",
        release_url="https://github.com/altnautica/ADOSDroneAgent/releases/tag/v0.2.0",
    )


@pytest.fixture
def agent_app():
    app = MagicMock()
    app.config = ADOSConfig()
    app.health = HealthMonitor()
    app.services = ServiceTracker()
    app._start_time = time.monotonic()
    app.uptime_seconds = 42.0
    app._vehicle_state = VehicleState()
    app._fc_connection = MagicMock()
    app._fc_connection.connected = False
    app._fc_connection.port = ""
    app._fc_connection.baud = 0
    app._tasks = []
    app._param_cache = None
    app.ota_updater = None
    # Auth middleware skips auth when unpaired
    app.pairing_manager.is_paired = False
    return app


@pytest.fixture
def client(agent_app):
    # Register OTA routes
    from ados.api.routes import ota
    fastapi_app = create_app(agent_app)
    fastapi_app.include_router(ota.router, prefix="/api")
    return TestClient(fastapi_app)


def test_get_ota_no_updater(client):
    resp = client.get("/api/ota")
    assert resp.status_code == 200
    data = resp.json()
    assert data["state"] == "idle"


def test_post_check_no_updater(client):
    resp = client.post("/api/ota/check")
    assert resp.status_code == 200
    data = resp.json()
    assert data["status"] == "error"


def test_post_install_no_updater(client):
    resp = client.post("/api/ota/install")
    assert resp.status_code == 200
    data = resp.json()
    assert data["status"] == "error"


def test_post_rollback_no_updater(client):
    resp = client.post("/api/ota/rollback")
    assert resp.status_code == 200
    data = resp.json()
    assert data["status"] == "error"


def test_get_ota_with_updater(agent_app, client):
    mock_updater = MagicMock()
    mock_updater.get_status.return_value = {
        "state": "idle",
        "current_version": "0.1.0",
        "error": "",
        "download": {"state": "idle", "percent": 0.0},
        "slots": {},
    }
    agent_app.ota_updater = mock_updater

    resp = client.get("/api/ota")
    assert resp.status_code == 200
    data = resp.json()
    assert data["current_version"] == "0.1.0"


def test_post_check_with_update(agent_app, client):
    manifest = _make_manifest()
    mock_updater = MagicMock()
    mock_updater.check = AsyncMock(return_value=manifest)
    agent_app.ota_updater = mock_updater

    resp = client.post("/api/ota/check")
    assert resp.status_code == 200
    data = resp.json()
    assert data["status"] == "update_available"
    assert data["version"] == "0.2.0"


def test_post_check_up_to_date(agent_app, client):
    mock_updater = MagicMock()
    mock_updater.check = AsyncMock(return_value=None)
    agent_app.ota_updater = mock_updater

    resp = client.post("/api/ota/check")
    assert resp.status_code == 200
    data = resp.json()
    assert data["status"] == "up_to_date"


def test_post_rollback_success(agent_app, client):
    mock_updater = MagicMock()
    mock_updater.rollback = AsyncMock(return_value=True)
    agent_app.ota_updater = mock_updater

    resp = client.post("/api/ota/rollback")
    assert resp.status_code == 200
    data = resp.json()
    assert data["status"] == "rolled_back"
