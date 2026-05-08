"""Tests for the recording fields surfaced on the video status routes.

Covers the C6 contract that the LCD video page consumes:

* ``GET /api/video`` returns ``recording``, ``recording_filename``,
  ``recording_started_at`` at the top level.
* ``GET /api/status/full`` mirrors the same fields inside the ``video``
  block.
* ``GET /api/v1/ground-station/status`` exposes ``video.recording`` +
  ``video.recording_filename``.
"""

from __future__ import annotations

from typing import Any
from unittest.mock import MagicMock

import pytest
from fastapi.testclient import TestClient

from ados.api.server import create_app
from ados.core.config import ADOSConfig
from ados.services.video.demo import DemoVideoPipeline
from tests.api_runtime_utils import build_api_runtime

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


class _FakeRecorder:
    """Tiny stand-in for the air-side VideoRecorder."""

    def __init__(self) -> None:
        self.recording = False
        self.is_recording = False
        self.current_path = ""
        self.current_filename: str | None = None
        self.started_at: str | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "recording": self.recording,
            "current_path": self.current_path,
            "current_filename": self.current_filename,
            "started_at": self.started_at,
            "recordings_dir": "/var/ados/recordings",
        }

    def set_active(self, filename: str, started_at: str) -> None:
        self.recording = True
        self.is_recording = True
        self.current_path = f"/var/ados/recordings/{filename}"
        self.current_filename = filename
        self.started_at = started_at

    def set_idle(self) -> None:
        self.recording = False
        self.is_recording = False
        self.current_path = ""
        self.current_filename = None
        self.started_at = None


class _FakeAirPipeline:
    """Pipeline shaped enough for the recording-block helper."""

    def __init__(self, recorder: _FakeRecorder) -> None:
        self.recorder = recorder
        self.camera_manager = MagicMock()
        self.camera_manager.cameras = []
        self.camera_manager.assignments = {}

    def get_status(self) -> dict[str, Any]:
        return {
            "state": "running",
            "encoder": "h264",
            "cameras": {"cameras": [], "assignments": {}},
            "recorder": self.recorder.to_dict(),
            "mediamtx": {"running": True, "webrtc_port": 8889},
            "cloud_push": False,
        }


# ---------------------------------------------------------------------------
# /api/video
# ---------------------------------------------------------------------------


@pytest.fixture
def air_recorder() -> _FakeRecorder:
    return _FakeRecorder()


@pytest.fixture
def air_pipeline(air_recorder: _FakeRecorder) -> _FakeAirPipeline:
    return _FakeAirPipeline(air_recorder)


@pytest.fixture
def air_client(air_pipeline: _FakeAirPipeline) -> TestClient:
    runtime = build_api_runtime(video_pipeline=air_pipeline)
    return TestClient(create_app(runtime))


def test_get_video_includes_recording_fields_when_idle(air_client: TestClient) -> None:
    """``GET /api/video`` always reports the recording block."""
    resp = air_client.get("/api/video")
    assert resp.status_code == 200
    body = resp.json()
    assert body["recording"] is False
    assert body["recording_filename"] is None
    assert body["recording_started_at"] is None


def test_get_video_includes_recording_fields_when_active(
    air_client: TestClient, air_recorder: _FakeRecorder
) -> None:
    air_recorder.set_active(
        "recording_20260507_143000.mp4", "2026-05-07T14:30:00+00:00"
    )
    resp = air_client.get("/api/video")
    assert resp.status_code == 200
    body = resp.json()
    assert body["recording"] is True
    assert body["recording_filename"] == "recording_20260507_143000.mp4"
    assert body["recording_started_at"] == "2026-05-07T14:30:00+00:00"


def test_get_video_no_pipeline_returns_idle_recording_block() -> None:
    """No pipeline = empty recording block, never absent fields."""
    runtime = build_api_runtime(video_pipeline=None)
    client = TestClient(create_app(runtime))
    resp = client.get("/api/video")
    assert resp.status_code == 200
    body = resp.json()
    assert body["recording"] is False
    assert body["recording_filename"] is None
    assert body["recording_started_at"] is None


def test_get_video_demo_pipeline_reports_recording_filename() -> None:
    """Demo pipeline path: filename is derived from the synthetic path."""
    runtime = build_api_runtime(video_pipeline=DemoVideoPipeline())
    client = TestClient(create_app(runtime))

    # Recording starts as idle.
    body = client.get("/api/video").json()
    assert body["recording"] is False
    assert body["recording_filename"] is None

    # Toggle recording on; filename should be the basename of the demo path.
    start = client.post("/api/video/record/start").json()
    assert start["recording"] is True
    assert start["recording_filename"] == "demo_recording.mp4"

    body = client.get("/api/video").json()
    assert body["recording"] is True
    assert body["recording_filename"] == "demo_recording.mp4"


# ---------------------------------------------------------------------------
# /api/status/full
# ---------------------------------------------------------------------------


def test_status_full_video_block_includes_recording(
    air_client: TestClient, air_recorder: _FakeRecorder
) -> None:
    """``/api/status/full`` mirrors the recording block inside ``video``."""
    air_recorder.set_active("rec.mp4", "2026-05-07T14:30:00+00:00")

    resp = air_client.get("/api/status/full")
    assert resp.status_code == 200
    body = resp.json()
    video_block = body.get("video") or {}
    assert video_block.get("recording") is True
    assert video_block.get("recording_filename") == "rec.mp4"
    assert video_block.get("recording_started_at") == "2026-05-07T14:30:00+00:00"


def test_status_full_video_block_idle_when_recorder_idle(
    air_client: TestClient,
) -> None:
    resp = air_client.get("/api/status/full")
    assert resp.status_code == 200
    body = resp.json()
    video_block = body.get("video") or {}
    assert video_block.get("recording") is False
    assert video_block.get("recording_filename") is None
    assert video_block.get("recording_started_at") is None


def test_status_full_no_pipeline_video_block_idle() -> None:
    runtime = build_api_runtime(video_pipeline=None)
    client = TestClient(create_app(runtime))
    resp = client.get("/api/status/full")
    assert resp.status_code == 200
    body = resp.json()
    video_block = body.get("video") or {}
    assert video_block.get("recording") is False
    assert video_block.get("recording_filename") is None
    assert video_block.get("recording_started_at") is None


# ---------------------------------------------------------------------------
# /api/v1/ground-station/status
# ---------------------------------------------------------------------------


class _FakeGroundRecorder:
    """Fake of GroundStationRecorder shaped for the status endpoint."""

    def __init__(self) -> None:
        self._active = False
        self._filename: str | None = None

    def is_active(self) -> bool:
        return self._active

    @property
    def current_filename(self) -> str | None:
        return self._filename

    def set_active(self, filename: str) -> None:
        self._active = True
        self._filename = filename

    def set_idle(self) -> None:
        self._active = False
        self._filename = None


@pytest.fixture
def ground_client(monkeypatch) -> TestClient:
    cfg = ADOSConfig()
    cfg.agent.profile = "ground_station"
    runtime = build_api_runtime(config=cfg)
    return TestClient(create_app(runtime))


def test_ground_status_video_block_idle(ground_client: TestClient, monkeypatch) -> None:
    from ados.api.routes import ground_station as gs

    fake = _FakeGroundRecorder()
    monkeypatch.setattr(gs, "_recorder", lambda: fake)

    resp = ground_client.get("/api/v1/ground-station/status")
    assert resp.status_code == 200
    body = resp.json()
    assert body["recording"] is False  # legacy field still there
    video_block = body.get("video") or {}
    assert video_block.get("recording") is False
    assert video_block.get("recording_filename") is None


def test_ground_status_video_block_active(ground_client: TestClient, monkeypatch) -> None:
    from ados.api.routes import ground_station as gs

    fake = _FakeGroundRecorder()
    fake.set_active("2026-05-07T14-30-00.mp4")
    monkeypatch.setattr(gs, "_recorder", lambda: fake)

    resp = ground_client.get("/api/v1/ground-station/status")
    assert resp.status_code == 200
    body = resp.json()
    assert body["recording"] is True
    video_block = body.get("video") or {}
    assert video_block.get("recording") is True
    assert video_block.get("recording_filename") == "2026-05-07T14-30-00.mp4"
