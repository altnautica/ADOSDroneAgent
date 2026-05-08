"""Tests for ``GET /api/video/cameras`` and ``POST /api/video/camera/switch``.

The C5 LCD video page consumes both endpoints: the page enumerates
cameras with the GET, and posts a role+device_path body when the
operator picks a different camera from the picker.
"""

from __future__ import annotations

import asyncio
from typing import Any
from unittest.mock import MagicMock

import pytest
from fastapi.testclient import TestClient

from ados.api.server import create_app
from ados.hal.camera import CameraInfo, CameraType
from ados.services.video.camera_mgr import CameraManager, CameraRole
from tests.api_runtime_utils import build_api_runtime

# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


def _make_cameras() -> list[CameraInfo]:
    return [
        CameraInfo(
            name="CSI-0 (imx219)",
            type=CameraType.CSI,
            device_path="/dev/video0",
            width=1920,
            height=1080,
        ),
        CameraInfo(
            name="USB Camera",
            type=CameraType.USB,
            device_path="/dev/video2",
            width=1280,
            height=720,
        ),
    ]


class _FakePipeline:
    """Pipeline with a real CameraManager and an instrumented switch."""

    def __init__(self) -> None:
        self.camera_manager = CameraManager()
        self.camera_manager.set_cameras(_make_cameras())
        self.camera_manager.auto_assign()
        # Recorder presence is required so /api/video doesn't crash.
        self.recorder = MagicMock()
        self.recorder.recording = False
        self.recorder.is_recording = False
        self.recorder.current_filename = None
        self.recorder.started_at = None
        self.recorder.current_path = ""
        self.recorder.to_dict.return_value = {
            "recording": False,
            "current_path": "",
            "current_filename": None,
            "started_at": None,
            "recordings_dir": "/tmp",
        }
        self.switch_calls: list[tuple[str, str]] = []
        # Drive concurrency tests.
        self._switch_lock = asyncio.Lock()
        self._switch_started: asyncio.Event | None = None
        self._switch_release: asyncio.Event | None = None

    def get_status(self) -> dict[str, Any]:
        return {
            "state": "running",
            "encoder": "h264",
            "cameras": self.camera_manager.to_dict(),
            "recorder": self.recorder.to_dict(),
            "mediamtx": {"running": False},
            "cloud_push": False,
        }

    async def restart_with_camera(self, role: str, device_path: str) -> None:
        async with self._switch_lock:
            if self._switch_started is not None:
                self._switch_started.set()
            if self._switch_release is not None:
                await self._switch_release.wait()
            self.switch_calls.append((role, device_path))
            try:
                role_enum = CameraRole(role)
            except ValueError as exc:
                raise ValueError(f"unknown camera role: {role}") from exc
            target = next(
                (c for c in self.camera_manager.cameras if c.device_path == device_path),
                None,
            )
            if target is None:
                raise LookupError(device_path)
            self.camera_manager.assign_role(target, role_enum)


@pytest.fixture
def fake_pipeline() -> _FakePipeline:
    return _FakePipeline()


@pytest.fixture
def client(fake_pipeline: _FakePipeline) -> TestClient:
    runtime = build_api_runtime(video_pipeline=fake_pipeline)
    return TestClient(create_app(runtime))


@pytest.fixture
def client_no_pipeline() -> TestClient:
    runtime = build_api_runtime(video_pipeline=None)
    return TestClient(create_app(runtime))


# ---------------------------------------------------------------------------
# GET /api/video/cameras
# ---------------------------------------------------------------------------


def test_list_cameras_returns_enumerated_cameras(
    client: TestClient, fake_pipeline: _FakePipeline
) -> None:
    resp = client.get("/api/video/cameras")
    assert resp.status_code == 200
    body = resp.json()
    cameras = body.get("cameras")
    assert isinstance(cameras, list)
    assert len(cameras) == 2
    paths = {c["device_path"] for c in cameras}
    assert paths == {"/dev/video0", "/dev/video2"}
    # Each entry must carry the keys the LCD page consumes.
    for entry in cameras:
        assert "type" in entry
        assert "label" in entry
        assert "width" in entry
        assert "height" in entry
    assignments = body.get("assignments") or {}
    assert assignments.get("primary") == "/dev/video0"


def test_list_cameras_falls_back_to_hal_when_no_pipeline(
    client_no_pipeline: TestClient, monkeypatch
) -> None:
    """No pipeline = HAL discovery fallback so the UI is never empty."""
    cams = _make_cameras()

    def _fake_discover() -> list[CameraInfo]:
        return cams

    monkeypatch.setattr(
        "ados.hal.camera.discover_cameras", _fake_discover, raising=True
    )

    resp = client_no_pipeline.get("/api/video/cameras")
    assert resp.status_code == 200
    body = resp.json()
    assert len(body["cameras"]) == 2
    assert body["assignments"] == {}


# ---------------------------------------------------------------------------
# POST /api/video/camera/switch — happy path
# ---------------------------------------------------------------------------


def test_switch_camera_calls_pipeline_restart(
    client: TestClient, fake_pipeline: _FakePipeline
) -> None:
    resp = client.post(
        "/api/video/camera/switch",
        json={"role": "primary", "device_path": "/dev/video2"},
    )
    assert resp.status_code == 200
    body = resp.json()
    assert body == {"ok": True, "restarting": True}
    assert fake_pipeline.switch_calls == [("primary", "/dev/video2")]
    primary = fake_pipeline.camera_manager.get_primary()
    assert primary is not None
    assert primary.device_path == "/dev/video2"


def test_switch_camera_secondary_role(
    client: TestClient, fake_pipeline: _FakePipeline
) -> None:
    resp = client.post(
        "/api/video/camera/switch",
        json={"role": "secondary", "device_path": "/dev/video0"},
    )
    assert resp.status_code == 200
    assert fake_pipeline.switch_calls == [("secondary", "/dev/video0")]


# ---------------------------------------------------------------------------
# POST /api/video/camera/switch — validation
# ---------------------------------------------------------------------------


def test_switch_camera_unknown_device_returns_400(client: TestClient) -> None:
    resp = client.post(
        "/api/video/camera/switch",
        json={"role": "primary", "device_path": "/dev/video99"},
    )
    assert resp.status_code == 400
    assert resp.json()["detail"] == "unknown camera"


def test_switch_camera_invalid_role_returns_422(client: TestClient) -> None:
    resp = client.post(
        "/api/video/camera/switch",
        json={"role": "thermal", "device_path": "/dev/video0"},
    )
    # Pydantic Literal rejection surfaces as 422.
    assert resp.status_code == 422


def test_switch_camera_missing_body_returns_422(client: TestClient) -> None:
    resp = client.post("/api/video/camera/switch", json={})
    assert resp.status_code == 422


def test_switch_camera_no_pipeline_returns_503(
    client_no_pipeline: TestClient,
) -> None:
    resp = client_no_pipeline.post(
        "/api/video/camera/switch",
        json={"role": "primary", "device_path": "/dev/video0"},
    )
    assert resp.status_code == 503


# ---------------------------------------------------------------------------
# POST /api/video/camera/switch — concurrency serialization
# ---------------------------------------------------------------------------


def test_switch_camera_sequential_calls_both_succeed(
    client: TestClient, fake_pipeline: _FakePipeline
) -> None:
    """Two back-to-back switches both land and the second binding wins.

    True async concurrency at the route level is exercised in
    ``test_pipeline_restart_with_camera_serialized`` against the
    pipeline's ``_switch_lock`` directly. This test guarantees the
    route surface itself never deadlocks across consecutive operator
    taps: the LCD page can fire camera_switch repeatedly and each
    request is processed cleanly without state leaking between them.
    """
    r1 = client.post(
        "/api/video/camera/switch",
        json={"role": "primary", "device_path": "/dev/video2"},
    )
    r2 = client.post(
        "/api/video/camera/switch",
        json={"role": "primary", "device_path": "/dev/video0"},
    )

    assert r1.status_code == 200
    assert r2.status_code == 200
    assert fake_pipeline.switch_calls == [
        ("primary", "/dev/video2"),
        ("primary", "/dev/video0"),
    ]
    primary = fake_pipeline.camera_manager.get_primary()
    assert primary is not None
    assert primary.device_path == "/dev/video0"


@pytest.mark.asyncio
async def test_switch_camera_concurrent_route_calls_serialize(
    fake_pipeline: _FakePipeline,
) -> None:
    """Direct-async drive: two coroutines into the route handler queue
    on the pipeline's switch lock, never overlap, and finish in
    arrival order. The fake pipeline already wraps its
    ``restart_with_camera`` body in ``_switch_lock``, mirroring the
    real pipeline's contract.
    """
    from ados.api.routes.video import CameraSwitchBody, switch_camera

    fake_pipeline._switch_started = asyncio.Event()
    fake_pipeline._switch_release = asyncio.Event()

    runtime = build_api_runtime(video_pipeline=fake_pipeline)
    from ados.api import deps

    deps.set_agent_app(runtime)

    body1 = CameraSwitchBody(role="primary", device_path="/dev/video2")
    body2 = CameraSwitchBody(role="primary", device_path="/dev/video0")

    async def _drive_first() -> dict[str, Any]:
        return await switch_camera(body1)

    async def _drive_second() -> dict[str, Any]:
        # Hold off until the first one has actually entered its critical
        # section so we can prove the second one is queued behind the
        # lock instead of running first by chance.
        await fake_pipeline._switch_started.wait()
        # Schedule a release after a short delay so the first call can
        # complete, the lock can hand over, and the second can run.
        async def _release_later() -> None:
            await asyncio.sleep(0.05)
            fake_pipeline._switch_release.set()

        asyncio.create_task(_release_later())
        return await switch_camera(body2)

    r1, r2 = await asyncio.gather(_drive_first(), _drive_second())

    assert r1["ok"] is True
    assert r2["ok"] is True
    # The second switch only landed after the first one released the
    # lock, so they must be in arrival order.
    assert fake_pipeline.switch_calls == [
        ("primary", "/dev/video2"),
        ("primary", "/dev/video0"),
    ]
