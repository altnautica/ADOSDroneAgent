"""Tests for the video API routes."""

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
from ados.services.video.demo import DemoVideoPipeline


@pytest.fixture
def agent_app_with_video():
    """Create a mock AgentApp with a demo video pipeline."""
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
    app._video_pipeline = DemoVideoPipeline()
    # Auth middleware skips auth when unpaired
    app.pairing_manager.is_paired = False
    return app


@pytest.fixture
def agent_app_no_video():
    """Create a mock AgentApp without video pipeline."""
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
    # Explicitly no _video_pipeline attribute
    del app._video_pipeline
    # Auth middleware skips auth when unpaired
    app.pairing_manager.is_paired = False
    return app


@pytest.fixture
def client_with_video(agent_app_with_video):
    fastapi_app = create_app(agent_app_with_video)
    return TestClient(fastapi_app)


@pytest.fixture
def client_no_video(agent_app_no_video):
    fastapi_app = create_app(agent_app_no_video)
    return TestClient(fastapi_app)


class TestVideoGetStatus:
    def test_with_pipeline(self, client_with_video):
        resp = client_with_video.get("/api/video")
        assert resp.status_code == 200
        data = resp.json()
        assert data["encoder"] == "demo"
        assert data["demo"] is True

    def test_without_pipeline(self, client_no_video):
        resp = client_no_video.get("/api/video")
        assert resp.status_code == 200
        data = resp.json()
        assert data["state"] == "not_initialized"


class TestVideoSnapshot:
    def test_snapshot_with_demo(self, client_with_video):
        resp = client_with_video.post("/api/video/snapshot")
        assert resp.status_code == 200
        data = resp.json()
        assert data["status"] == "captured"
        assert data["path"].endswith(".jpg")

    def test_snapshot_no_pipeline(self, client_no_video):
        resp = client_no_video.post("/api/video/snapshot")
        assert resp.status_code == 200
        data = resp.json()
        assert "error" in data


class TestVideoRecording:
    def test_start_recording_demo(self, client_with_video):
        resp = client_with_video.post("/api/video/record/start")
        assert resp.status_code == 200
        data = resp.json()
        assert data["status"] == "recording"
        assert data["path"] != ""

    def test_stop_recording_demo(self, client_with_video):
        # Start first
        client_with_video.post("/api/video/record/start")
        resp = client_with_video.post("/api/video/record/stop")
        assert resp.status_code == 200
        data = resp.json()
        assert data["status"] == "stopped"

    def test_start_recording_no_pipeline(self, client_no_video):
        resp = client_no_video.post("/api/video/record/start")
        assert resp.status_code == 200
        data = resp.json()
        assert "error" in data

    def test_stop_recording_no_pipeline(self, client_no_video):
        resp = client_no_video.post("/api/video/record/stop")
        assert resp.status_code == 200
        data = resp.json()
        assert "error" in data
