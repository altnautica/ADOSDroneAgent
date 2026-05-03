"""Tests for the video API routes."""

from __future__ import annotations

import pytest
from fastapi.testclient import TestClient

from ados.api.server import create_app
from ados.services.video.demo import DemoVideoPipeline
from tests.api_runtime_utils import build_api_runtime


@pytest.fixture
def agent_app_with_video():
    """Create an API runtime double with a demo video pipeline."""
    return build_api_runtime(video_pipeline=DemoVideoPipeline())


@pytest.fixture
def agent_app_no_video():
    """Create an API runtime double without a video pipeline."""
    return build_api_runtime(video_pipeline=None)


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
