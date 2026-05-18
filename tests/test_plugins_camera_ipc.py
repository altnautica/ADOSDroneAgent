"""Tests for the plugin camera IPC surface.

Covers the in-host primitives that back the ``ctx.camera`` facade:
  * ``CameraClaimTracker.claim`` / ``release`` / exclusive contention
  * ``publish_frame`` / ``latest_frame`` cache
  * ``handle_camera_get_frame`` happy path + every error gate
"""

from __future__ import annotations

import pytest

from ados.plugins.ipc_server import _RpcError as RpcError
from ados.plugins.ipc import handlers
from ados.plugins.ipc.host_services import (
    CameraClaim,
    CameraClaimTracker,
    CameraFrame,
    HostServices,
    default_host_services,
)


def _make_frame(*, format: str = "nv12", fid: int = 1) -> CameraFrame:
    # NV12 stride for 64×36 is 64×36 luma + 64×18 chroma = 3456 bytes.
    return CameraFrame(
        frame_id=fid,
        width=64,
        height=36,
        format=format,
        data=b"\x00" * 3456,
        ts_ns=1_000_000_000 + fid,
    )


# ── CameraClaimTracker ───────────────────────────────────────


def test_claim_records_holder() -> None:
    tracker = CameraClaimTracker()
    claim = tracker.claim("plugin.a", "/dev/video0", exclusive=True)
    assert isinstance(claim, CameraClaim)
    assert tracker.holder("/dev/video0") == "plugin.a"


def test_exclusive_claim_blocks_another_plugin() -> None:
    tracker = CameraClaimTracker()
    tracker.claim("plugin.a", "/dev/video0", exclusive=True)
    with pytest.raises(PermissionError):
        tracker.claim("plugin.b", "/dev/video0", exclusive=True)


def test_same_plugin_can_reclaim() -> None:
    tracker = CameraClaimTracker()
    tracker.claim("plugin.a", "/dev/video0", exclusive=True)
    # Re-claim from the same plugin must succeed (idempotent).
    tracker.claim("plugin.a", "/dev/video0", exclusive=True)
    assert tracker.holder("/dev/video0") == "plugin.a"


def test_release_clears_holder_and_frame_cache() -> None:
    tracker = CameraClaimTracker()
    tracker.claim("plugin.a", "/dev/video0", exclusive=True)
    tracker.publish_frame("/dev/video0", _make_frame())
    tracker.release("plugin.a", "/dev/video0")
    assert tracker.holder("/dev/video0") is None
    assert tracker.latest_frame("/dev/video0") is None


def test_release_by_wrong_plugin_raises() -> None:
    tracker = CameraClaimTracker()
    tracker.claim("plugin.a", "/dev/video0", exclusive=True)
    with pytest.raises(PermissionError):
        tracker.release("plugin.b", "/dev/video0")


def test_release_when_not_held_is_noop() -> None:
    tracker = CameraClaimTracker()
    # No prior claim — release must not raise.
    tracker.release("plugin.a", "/dev/video0")


def test_release_plugin_clears_all_paths() -> None:
    tracker = CameraClaimTracker()
    tracker.claim("plugin.a", "/dev/video0", exclusive=True)
    tracker.claim("plugin.a", "/dev/video1", exclusive=True)
    tracker.publish_frame("/dev/video0", _make_frame())
    tracker.release_plugin("plugin.a")
    assert tracker.holder("/dev/video0") is None
    assert tracker.holder("/dev/video1") is None
    assert tracker.latest_frame("/dev/video0") is None


def test_publish_and_read_latest_frame() -> None:
    tracker = CameraClaimTracker()
    tracker.claim("plugin.a", "/dev/video0", exclusive=True)
    frame = _make_frame(fid=42)
    tracker.publish_frame("/dev/video0", frame)
    got = tracker.latest_frame("/dev/video0")
    assert got is not None
    assert got.frame_id == 42
    assert got.format == "nv12"


# ── handle_camera_get_frame ──────────────────────────────────


class _StubSession:
    def __init__(self, plugin_id: str) -> None:
        self.plugin_id = plugin_id


class _StubServer:
    def __init__(self, host: HostServices) -> None:
        self.host = host


class _StubEnvelope:
    def __init__(self, args: dict) -> None:
        self.args = args


def _server_with_claim(plugin_id: str = "plugin.a") -> _StubServer:
    host = default_host_services()
    host.cameras.claim(plugin_id, "/dev/video0", exclusive=True)
    return _StubServer(host)


@pytest.mark.asyncio
async def test_get_frame_happy_path_returns_cached_frame() -> None:
    server = _server_with_claim()
    server.host.cameras.publish_frame("/dev/video0", _make_frame(fid=7))
    out = await handlers.handle_camera_get_frame(
        server,  # type: ignore[arg-type]
        _StubSession("plugin.a"),  # type: ignore[arg-type]
        _StubEnvelope({"device_path": "/dev/video0", "format": "nv12"}),  # type: ignore[arg-type]
    )
    assert out["frame_id"] == 7
    assert out["width"] == 64
    assert out["height"] == 36
    assert out["format"] == "nv12"
    assert isinstance(out["data"], bytes)
    assert out["stale"] is False


@pytest.mark.asyncio
async def test_get_frame_rejects_empty_device_path() -> None:
    server = _server_with_claim()
    with pytest.raises(RpcError):
        await handlers.handle_camera_get_frame(
            server,  # type: ignore[arg-type]
            _StubSession("plugin.a"),  # type: ignore[arg-type]
            _StubEnvelope({"device_path": "", "format": "nv12"}),  # type: ignore[arg-type]
        )


@pytest.mark.asyncio
async def test_get_frame_rejects_unsupported_format() -> None:
    server = _server_with_claim()
    server.host.cameras.publish_frame("/dev/video0", _make_frame())
    with pytest.raises(RpcError):
        await handlers.handle_camera_get_frame(
            server,  # type: ignore[arg-type]
            _StubSession("plugin.a"),  # type: ignore[arg-type]
            _StubEnvelope(
                {"device_path": "/dev/video0", "format": "h264"},
            ),  # type: ignore[arg-type]
        )


@pytest.mark.asyncio
async def test_get_frame_rejects_when_not_claimed() -> None:
    host = default_host_services()
    server = _StubServer(host)
    with pytest.raises(RpcError):
        await handlers.handle_camera_get_frame(
            server,  # type: ignore[arg-type]
            _StubSession("plugin.a"),  # type: ignore[arg-type]
            _StubEnvelope(
                {"device_path": "/dev/video0", "format": "nv12"},
            ),  # type: ignore[arg-type]
        )


@pytest.mark.asyncio
async def test_get_frame_rejects_when_held_by_another_plugin() -> None:
    server = _server_with_claim(plugin_id="plugin.a")
    server.host.cameras.publish_frame("/dev/video0", _make_frame())
    with pytest.raises(RpcError):
        await handlers.handle_camera_get_frame(
            server,  # type: ignore[arg-type]
            _StubSession("plugin.b"),  # type: ignore[arg-type]
            _StubEnvelope(
                {"device_path": "/dev/video0", "format": "nv12"},
            ),  # type: ignore[arg-type]
        )


@pytest.mark.asyncio
async def test_get_frame_rejects_when_no_frame_published_yet() -> None:
    server = _server_with_claim()
    # No publish_frame call — the supervisor returns an error rather
    # than echoing an empty buffer.
    with pytest.raises(RpcError):
        await handlers.handle_camera_get_frame(
            server,  # type: ignore[arg-type]
            _StubSession("plugin.a"),  # type: ignore[arg-type]
            _StubEnvelope(
                {"device_path": "/dev/video0", "format": "nv12"},
            ),  # type: ignore[arg-type]
        )


@pytest.mark.asyncio
async def test_get_frame_rejects_format_mismatch_with_pipeline() -> None:
    server = _server_with_claim()
    # Pipeline produced rgb888 but plugin asked for nv12.
    server.host.cameras.publish_frame(
        "/dev/video0", _make_frame(format="rgb888")
    )
    with pytest.raises(RpcError):
        await handlers.handle_camera_get_frame(
            server,  # type: ignore[arg-type]
            _StubSession("plugin.a"),  # type: ignore[arg-type]
            _StubEnvelope(
                {"device_path": "/dev/video0", "format": "nv12"},
            ),  # type: ignore[arg-type]
        )


@pytest.mark.asyncio
async def test_get_frame_negative_timeout_rejected() -> None:
    server = _server_with_claim()
    server.host.cameras.publish_frame("/dev/video0", _make_frame())
    with pytest.raises(RpcError):
        await handlers.handle_camera_get_frame(
            server,  # type: ignore[arg-type]
            _StubSession("plugin.a"),  # type: ignore[arg-type]
            _StubEnvelope(
                {
                    "device_path": "/dev/video0",
                    "format": "nv12",
                    "timeout_ms": -5,
                },
            ),  # type: ignore[arg-type]
        )


# ── handle_camera_release ────────────────────────────────────


@pytest.mark.asyncio
async def test_release_handler_clears_holder() -> None:
    server = _server_with_claim()
    out = await handlers.handle_camera_release(
        server,  # type: ignore[arg-type]
        _StubSession("plugin.a"),  # type: ignore[arg-type]
        _StubEnvelope({"device_path": "/dev/video0"}),  # type: ignore[arg-type]
    )
    assert out == {"released": True, "device_path": "/dev/video0"}
    assert server.host.cameras.holder("/dev/video0") is None


@pytest.mark.asyncio
async def test_release_handler_rejects_other_plugin() -> None:
    server = _server_with_claim(plugin_id="plugin.a")
    with pytest.raises(RpcError):
        await handlers.handle_camera_release(
            server,  # type: ignore[arg-type]
            _StubSession("plugin.b"),  # type: ignore[arg-type]
            _StubEnvelope({"device_path": "/dev/video0"}),  # type: ignore[arg-type]
        )
