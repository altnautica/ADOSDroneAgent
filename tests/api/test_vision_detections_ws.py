"""Tests for the live vision-detection WebSocket bridge.

The route connects to the engine's ``vision-detections.sock`` broadcast,
decodes each length-prefixed msgpack ``DetectionBatch``, and forwards it
to the WebSocket peer as JSON. These tests stand up a tiny Unix-socket
server (standing in for the Rust engine) that writes framed batches, then
assert the JSON the browser would receive.
"""

from __future__ import annotations

import asyncio
import struct
import tempfile
import threading
from pathlib import Path

import msgpack
import pytest
from fastapi.testclient import TestClient

from ados.api.routes import vision_detections as route
from ados.api.server import create_app
from tests.api_runtime_utils import build_api_runtime


def _frame(batch: dict) -> bytes:
    """4-byte big-endian length prefix + msgpack body, matching the engine."""
    body = msgpack.packb(batch, use_bin_type=True)
    return struct.pack("!I", len(body)) + body


SAMPLE_BATCH = {
    "model_id": "com.example.weeds",
    "camera_id": "uvc-0",
    "frame_id": 7,
    "ts_ms": 1_700_000_000_000,
    "detections": [
        {
            "bbox": {"x": 12.0, "y": 20.0, "width": 64.0, "height": 32.0},
            "class_label": "weed",
            "confidence": 0.87,
            "track_id": 3,
        }
    ],
}


class _FakeEngineSocket:
    """A minimal stand-in for the engine's vision-detections.sock.

    Binds a Unix socket and, on each client connect, writes the configured
    batch frames then keeps the connection open (mirrors a last-state
    broadcast that has one batch to replay).
    """

    def __init__(self, sock_path: Path, frames: list[bytes]) -> None:
        self._sock_path = sock_path
        self._frames = frames
        self._loop = asyncio.new_event_loop()
        self._server: asyncio.AbstractServer | None = None
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._ready = threading.Event()

    async def _handle(self, reader, writer):  # noqa: ANN001
        for f in self._frames:
            writer.write(f)
        await writer.drain()
        # Hold the connection open so the route stays in its read loop.
        try:
            while not reader.at_eof():
                await asyncio.sleep(0.05)
        except Exception:
            pass

    def _run(self) -> None:
        asyncio.set_event_loop(self._loop)

        async def _start() -> None:
            self._server = await asyncio.start_unix_server(
                self._handle, path=str(self._sock_path)
            )
            self._ready.set()

        self._loop.run_until_complete(_start())
        self._loop.run_forever()

    def start(self) -> None:
        self._thread.start()
        assert self._ready.wait(timeout=5), "fake engine socket did not bind"

    def stop(self) -> None:
        def _shutdown() -> None:
            if self._server is not None:
                self._server.close()
            self._loop.stop()

        self._loop.call_soon_threadsafe(_shutdown)
        self._thread.join(timeout=5)


@pytest.fixture
def short_sock_path():
    """A short Unix-socket path under the system temp dir.

    macOS caps ``AF_UNIX`` paths at ~104 bytes, and pytest's ``tmp_path``
    nests too deep, so bind under a freshly made short dir instead.
    """
    base = Path(tempfile.mkdtemp(prefix="advws-"))
    path = base / "v.sock"
    yield path
    try:
        if path.exists():
            path.unlink()
        base.rmdir()
    except OSError:
        pass


@pytest.fixture
def unpaired_client(short_sock_path, monkeypatch):
    """Unpaired agent → open WS posture (no ticket needed)."""
    monkeypatch.setattr(
        route, "VISION_DETECTIONS_SOCK", short_sock_path, raising=False
    )
    app_double = build_api_runtime(uptime_seconds=0.0)
    app_double.pairing_manager.is_paired = False
    app_double.pairing_manager.api_key = "test-pair-key"
    return TestClient(create_app(app_double)), short_sock_path


@pytest.fixture
def paired_client(short_sock_path, monkeypatch):
    """Paired agent → the route must enforce the WS auth contract."""
    monkeypatch.setattr(
        route, "VISION_DETECTIONS_SOCK", short_sock_path, raising=False
    )
    app_double = build_api_runtime(uptime_seconds=0.0)
    app_double.pairing_manager.is_paired = True
    app_double.pairing_manager.api_key = "valid-pair-key"
    app_double.pairing_manager.validate_key = lambda k: k == "valid-pair-key"
    return TestClient(create_app(app_double)), short_sock_path


def test_forwards_batch_as_json(unpaired_client):
    client, sock_path = unpaired_client
    engine = _FakeEngineSocket(sock_path, [_frame(SAMPLE_BATCH)])
    engine.start()
    try:
        with client.websocket_connect("/api/vision/detections/ws") as ws:
            got = ws.receive_json()
        assert got["model_id"] == "com.example.weeds"
        assert got["camera_id"] == "uvc-0"
        assert got["frame_id"] == 7
        assert got["ts_ms"] == 1_700_000_000_000
        assert len(got["detections"]) == 1
        det = got["detections"][0]
        assert det["class_label"] == "weed"
        assert det["confidence"] == pytest.approx(0.87)
        assert det["track_id"] == 3
        assert det["bbox"] == {"x": 12.0, "y": 20.0, "width": 64.0, "height": 32.0}
    finally:
        engine.stop()


def test_forwards_multiple_batches_in_order(unpaired_client):
    client, sock_path = unpaired_client
    second = dict(SAMPLE_BATCH, frame_id=8, detections=[])
    engine = _FakeEngineSocket(sock_path, [_frame(SAMPLE_BATCH), _frame(second)])
    engine.start()
    try:
        with client.websocket_connect("/api/vision/detections/ws") as ws:
            a = ws.receive_json()
            b = ws.receive_json()
        assert a["frame_id"] == 7
        assert b["frame_id"] == 8
        assert b["detections"] == []
    finally:
        engine.stop()


def test_closes_cleanly_when_socket_absent(unpaired_client):
    """No engine socket present → the route closes rather than hangs."""
    from starlette.websockets import WebSocketDisconnect as _WSDisconnect

    client, _sock_path = unpaired_client
    # No _FakeEngineSocket started, so the connect retries then closes.
    with pytest.raises(_WSDisconnect):
        with client.websocket_connect("/api/vision/detections/ws") as ws:
            ws.receive_json()


def test_paired_no_key_rejected(paired_client):
    from starlette.websockets import WebSocketDisconnect as _WSDisconnect

    client, sock_path = paired_client
    engine = _FakeEngineSocket(sock_path, [_frame(SAMPLE_BATCH)])
    engine.start()
    try:
        with pytest.raises(_WSDisconnect) as excinfo:
            with client.websocket_connect("/api/vision/detections/ws") as ws:
                ws.receive_json()
        assert excinfo.value.code == 4401
    finally:
        engine.stop()


def test_paired_valid_header_accepts(paired_client):
    client, sock_path = paired_client
    engine = _FakeEngineSocket(sock_path, [_frame(SAMPLE_BATCH)])
    engine.start()
    try:
        with client.websocket_connect(
            "/api/vision/detections/ws",
            headers={"X-ADOS-Key": "valid-pair-key"},
        ) as ws:
            got = ws.receive_json()
        assert got["frame_id"] == 7
    finally:
        engine.stop()
