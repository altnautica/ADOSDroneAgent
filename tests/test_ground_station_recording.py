"""Tests for the ground-station recording service + REST endpoints.

The ffmpeg subprocess is mocked at the recorder service level rather
than at the asyncio.create_subprocess_exec level so the lifecycle
plumbing (lock, watcher task, returncode handling) stays exercised.
"""

from __future__ import annotations

from typing import Any
from unittest.mock import MagicMock

import pytest
from fastapi.testclient import TestClient

from ados.api.server import create_app
from ados.core.config import ADOSConfig
from tests.api_runtime_utils import build_api_runtime

GS_PREFIX = "/api/v1/ground-station"


def _build_agent_app(profile: str = "ground_station") -> Any:
    cfg = ADOSConfig()
    cfg.agent.profile = profile
    return build_api_runtime(config=cfg)


@pytest.fixture
def agent_app():
    return _build_agent_app("ground_station")


@pytest.fixture
def drone_agent_app():
    return _build_agent_app("auto")


@pytest.fixture
def client(agent_app):
    return TestClient(create_app(agent_app))


@pytest.fixture
def drone_client(drone_agent_app):
    return TestClient(create_app(drone_agent_app))


class _FakeRecorder:
    """Fake GroundStationRecorder that records calls and toggles state."""

    def __init__(self) -> None:
        self._active = False
        self._current = None
        self._listing: list[dict[str, Any]] = []
        self.start_calls: list[str | None] = []
        self.stop_calls = 0
        self._next_start_error: Any = None
        self._next_stop_error: Any = None

    def is_active(self) -> bool:
        return self._active

    @property
    def current_filename(self) -> str | None:
        return self._current

    async def start(self, filename_hint: str | None = None) -> dict[str, Any]:
        self.start_calls.append(filename_hint)
        if self._next_start_error is not None:
            err = self._next_start_error
            self._next_start_error = None
            raise err
        self._active = True
        self._current = "2026-05-07T14-30-00.mp4"
        return {
            "filename": self._current,
            "started_at": "2026-05-07T14:30:00+00:00",
            "path": f"/var/ados/recordings/{self._current}",
        }

    async def stop(self) -> dict[str, Any]:
        self.stop_calls += 1
        if self._next_stop_error is not None:
            err = self._next_stop_error
            self._next_stop_error = None
            raise err
        filename = self._current or "unknown.mp4"
        self._active = False
        self._current = None
        return {
            "filename": filename,
            "stopped_at": "2026-05-07T14:31:00+00:00",
            "duration_seconds": 60.0,
            "size_bytes": 1024,
        }

    def list_recordings(self) -> list[Any]:
        # Return objects with to_dict() to mirror the real shape.
        out: list[Any] = []
        for row in self._listing:
            m = MagicMock()
            m.to_dict.return_value = row
            out.append(m)
        return out


@pytest.fixture
def fake_recorder(monkeypatch):
    """Install a fake recorder on the route-package singleton accessor."""
    from ados.api.routes import ground_station as gs

    fake = _FakeRecorder()
    monkeypatch.setattr(gs, "_recorder", lambda: fake)
    return fake


# ---------------------------------------------------------------------------
# /recording/start
# ---------------------------------------------------------------------------


def test_recording_start_returns_metadata(client, fake_recorder):
    """Happy path: POST /recording/start spawns a recording and returns metadata."""
    resp = client.post(f"{GS_PREFIX}/recording/start", json={})
    assert resp.status_code == 200
    body = resp.json()
    assert body["filename"].endswith(".mp4")
    assert "started_at" in body
    assert body["path"].startswith("/var/ados/recordings/")
    assert fake_recorder.is_active() is True
    assert fake_recorder.start_calls == [None]


def test_recording_start_conflict_when_already_active(client, fake_recorder):
    """409 when start fires while a recording is already in flight."""
    from ados.services.ground_station.recorder import RecorderError

    fake_recorder._next_start_error = RecorderError(
        "E_RECORDING_ACTIVE", "a recording is already in progress"
    )
    resp = client.post(f"{GS_PREFIX}/recording/start", json={})
    assert resp.status_code == 409
    assert resp.json()["detail"]["error"]["code"] == "E_RECORDING_ACTIVE"


def test_recording_start_ffmpeg_missing_returns_503(client, fake_recorder):
    """503 when the ffmpeg binary is not present on the box."""
    from ados.services.ground_station.recorder import RecorderError

    fake_recorder._next_start_error = RecorderError(
        "E_FFMPEG_NOT_FOUND", "ffmpeg binary not on PATH"
    )
    resp = client.post(f"{GS_PREFIX}/recording/start", json={})
    assert resp.status_code == 503
    assert resp.json()["detail"]["error"]["code"] == "E_FFMPEG_NOT_FOUND"


def test_recording_start_passes_filename_hint(client, fake_recorder):
    """The optional filename_hint reaches the recorder."""
    resp = client.post(
        f"{GS_PREFIX}/recording/start", json={"filename_hint": "test_flight"}
    )
    assert resp.status_code == 200
    assert fake_recorder.start_calls == ["test_flight"]


# ---------------------------------------------------------------------------
# /recording/stop
# ---------------------------------------------------------------------------


def test_recording_stop_returns_summary(client, fake_recorder):
    """Stopping an active recording returns duration + size."""
    # Prime the fake into "active" state.
    client.post(f"{GS_PREFIX}/recording/start", json={})

    resp = client.post(f"{GS_PREFIX}/recording/stop")
    assert resp.status_code == 200
    body = resp.json()
    assert "filename" in body
    assert body["duration_seconds"] >= 0
    assert "size_bytes" in body
    assert fake_recorder.stop_calls == 1
    assert fake_recorder.is_active() is False


def test_recording_stop_conflict_when_idle(client, fake_recorder):
    """409 when stop is called and nothing is recording."""
    from ados.services.ground_station.recorder import RecorderError

    fake_recorder._next_stop_error = RecorderError(
        "E_RECORDING_NOT_ACTIVE", "no recording is currently active"
    )
    resp = client.post(f"{GS_PREFIX}/recording/stop")
    assert resp.status_code == 409
    assert resp.json()["detail"]["error"]["code"] == "E_RECORDING_NOT_ACTIVE"


# ---------------------------------------------------------------------------
# /recording/list
# ---------------------------------------------------------------------------


def test_recording_list_returns_files(client, fake_recorder):
    """List returns the files the recorder advertises plus the active flag."""
    fake_recorder._listing = [
        {"filename": "a.mp4", "size_bytes": 1000, "mtime": 1.0},
        {"filename": "b.mp4", "size_bytes": 2000, "mtime": 2.0},
    ]
    resp = client.get(f"{GS_PREFIX}/recording/list")
    assert resp.status_code == 200
    body = resp.json()
    assert body["recording"] is False
    assert body["current_filename"] is None
    assert len(body["items"]) == 2
    assert body["items"][0]["filename"] == "a.mp4"


# ---------------------------------------------------------------------------
# Profile gate
# ---------------------------------------------------------------------------


def test_recording_profile_gate_drone(drone_client):
    """Drone profile gets 404 with the profile mismatch error code."""
    resp = drone_client.post(f"{GS_PREFIX}/recording/start", json={})
    assert resp.status_code == 404
    assert resp.json()["detail"]["error"]["code"] == "E_PROFILE_MISMATCH"

    resp = drone_client.get(f"{GS_PREFIX}/recording/list")
    assert resp.status_code == 404


# ---------------------------------------------------------------------------
# /status surfaces live recording state
# ---------------------------------------------------------------------------


def test_status_recording_field_reflects_recorder(client, fake_recorder, monkeypatch):
    """GET /status returns recording=true while a recording is active."""
    from unittest.mock import AsyncMock

    from ados.api.routes import ground_station as gs

    fake_pm = MagicMock()
    fake_pm.status = AsyncMock(
        return_value={"paired": False, "key_fingerprint": None}
    )
    monkeypatch.setattr(gs, "_pair_manager", lambda: fake_pm)

    # Prime the fake recorder into the active state.
    client.post(f"{GS_PREFIX}/recording/start", json={})
    assert fake_recorder.is_active() is True

    resp = client.get(f"{GS_PREFIX}/status")
    assert resp.status_code == 200
    assert resp.json()["recording"] is True

    client.post(f"{GS_PREFIX}/recording/stop")
    resp = client.get(f"{GS_PREFIX}/status")
    assert resp.json()["recording"] is False


def test_status_recording_resilient_to_recorder_fault(client, monkeypatch):
    """A broken recorder accessor never crashes /status; field falls back to false."""
    from unittest.mock import AsyncMock

    from ados.api.routes import ground_station as gs

    fake_pm = MagicMock()
    fake_pm.status = AsyncMock(
        return_value={"paired": False, "key_fingerprint": None}
    )
    monkeypatch.setattr(gs, "_pair_manager", lambda: fake_pm)

    def _boom():
        raise RuntimeError("recorder service unavailable")

    monkeypatch.setattr(gs, "_recorder", _boom)

    resp = client.get(f"{GS_PREFIX}/status")
    assert resp.status_code == 200
    assert resp.json()["recording"] is False
