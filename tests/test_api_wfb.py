"""Tests for WFB-ng API routes."""

from __future__ import annotations

import time
from unittest.mock import MagicMock

import pytest
from fastapi.testclient import TestClient

from ados.api.server import create_app
from ados.core.config import ADOSConfig
from ados.core.health import HealthMonitor
from ados.core.main import ServiceTracker
from ados.services.mavlink.state import VehicleState
from ados.services.wfb.demo import DemoWfbManager
from ados.services.wfb.link_quality import LinkStats
from ados.services.wfb.manager import LinkState


@pytest.fixture
def agent_app():
    """Create a mock AgentApp with WFB manager."""
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
    app._wfb_manager = None
    return app


@pytest.fixture
def client(agent_app):
    """FastAPI test client."""
    fastapi_app = create_app(agent_app)
    return TestClient(fastapi_app)


def test_wfb_status_no_manager(client):
    """GET /api/wfb returns disabled state when no WFB manager."""
    resp = client.get("/api/wfb")
    assert resp.status_code == 200
    data = resp.json()
    assert data["state"] == "disabled"
    assert data["rssi_dbm"] == -100.0


def test_wfb_status_with_demo(agent_app):
    """GET /api/wfb returns demo data when demo manager is set."""
    demo = DemoWfbManager()
    demo._state = LinkState.CONNECTED
    stats = LinkStats(rssi_dbm=-55.0, packets_received=1000, loss_percent=0.5)
    demo._monitor._latest = stats
    agent_app._wfb_manager = demo

    fastapi_app = create_app(agent_app)
    client = TestClient(fastapi_app)

    resp = client.get("/api/wfb")
    assert resp.status_code == 200
    data = resp.json()
    assert data["state"] == "connected"
    assert data["rssi_dbm"] == -55.0


def test_wfb_history_no_manager(client):
    """GET /api/wfb/history returns empty when no manager."""
    resp = client.get("/api/wfb/history")
    assert resp.status_code == 200
    data = resp.json()
    assert data["samples"] == []
    assert data["count"] == 0


def test_wfb_history_with_data(agent_app):
    """GET /api/wfb/history returns samples from monitor."""
    demo = DemoWfbManager()
    # Feed some stats
    for i in range(5):
        line = (
            f"rssi_min=-{50+i} rssi_avg=-{48+i} rssi_max=-{46+i} "
            f"packets={1000+i} lost={i} fec_rec=0 fec_fail=0"
        )
        demo.monitor.feed_line(line)
    agent_app._wfb_manager = demo

    fastapi_app = create_app(agent_app)
    client = TestClient(fastapi_app)

    resp = client.get("/api/wfb/history?seconds=60")
    assert resp.status_code == 200
    data = resp.json()
    assert data["count"] == 5
    assert len(data["samples"]) == 5
    assert "rssi_dbm" in data["samples"][0]


def test_wfb_set_channel_valid(agent_app):
    """POST /api/wfb/channel with valid channel."""
    demo = DemoWfbManager()
    agent_app._wfb_manager = demo

    fastapi_app = create_app(agent_app)
    client = TestClient(fastapi_app)

    resp = client.post("/api/wfb/channel", json={"channel": 36})
    assert resp.status_code == 200
    data = resp.json()
    assert data["status"] == "ok"
    assert data["channel"] == 36
    assert data["frequency_mhz"] == 5180


def test_wfb_set_channel_invalid(agent_app):
    """POST /api/wfb/channel with invalid channel returns 400."""
    demo = DemoWfbManager()
    agent_app._wfb_manager = demo

    fastapi_app = create_app(agent_app)
    client = TestClient(fastapi_app)

    resp = client.post("/api/wfb/channel", json={"channel": 999})
    assert resp.status_code == 400


def test_wfb_set_channel_no_manager(client):
    """POST /api/wfb/channel without manager returns 503."""
    resp = client.post("/api/wfb/channel", json={"channel": 149})
    assert resp.status_code == 503
