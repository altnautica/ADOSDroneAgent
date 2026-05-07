"""Tests for the ground-station /camera/switch endpoint.

Covers:
* Single-camera (or unspecified) drone returns 501 with the
  not-supported reason.
* Multi-camera drone accepts the switch and pushes a real
  MAV_CMD_SET_CAMERA_SOURCE COMMAND_LONG frame onto the MAVLink IPC
  bus. The test stands up a real ``MavlinkIPCServer`` so the wire
  framing is exercised end-to-end.
* Drone profile is rejected with E_PROFILE_MISMATCH.
* A malformed camera_id (non-numeric or out-of-range) returns 400.
* IPC bus down returns 503 so the GCS can retry.
"""

from __future__ import annotations

import asyncio
import struct
import tempfile
import threading
from pathlib import Path

import pytest
from fastapi.testclient import TestClient
from pymavlink.dialects.v20 import common as mavlink2

from ados.api.server import create_app
from ados.core.config import ADOSConfig
from ados.core.ipc import MavlinkIPCServer
from tests.api_runtime_utils import build_api_runtime

GS_PREFIX = "/api/v1/ground-station"
SWITCH_PATH = f"{GS_PREFIX}/camera/switch"


def _short_sock_path(name: str) -> Path:
    """Return a short Unix socket path that fits AF_UNIX's ~104-char limit."""
    base = Path(tempfile.mkdtemp(prefix="ados-cam-"))
    return base / name


def _build_client(profile: str = "ground_station") -> TestClient:
    cfg = ADOSConfig()
    cfg.agent.profile = profile
    runtime = build_api_runtime(config=cfg)
    return TestClient(create_app(runtime))


@pytest.fixture
def client() -> TestClient:
    return _build_client("ground_station")


@pytest.fixture
def drone_client() -> TestClient:
    return _build_client("auto")


class _IpcHarness:
    """Run a real MavlinkIPCServer in a thread + capture uplink bytes."""

    def __init__(self, sock_path: Path) -> None:
        self.sock_path = sock_path
        self._server: MavlinkIPCServer | None = None
        self._loop: asyncio.AbstractEventLoop | None = None
        self._thread: threading.Thread | None = None
        self.received: list[bytes] = []

    def start(self) -> None:
        ready = threading.Event()

        def _run() -> None:
            loop = asyncio.new_event_loop()
            asyncio.set_event_loop(loop)
            self._loop = loop
            self._server = MavlinkIPCServer(sock_path=self.sock_path)
            self._server.set_command_handler(self.received.append)
            loop.run_until_complete(self._server.start())
            ready.set()
            loop.run_forever()

        self._thread = threading.Thread(target=_run, daemon=True)
        self._thread.start()
        assert ready.wait(5.0), "ipc server did not start"

    def stop(self) -> None:
        if self._loop and self._server:
            fut = asyncio.run_coroutine_threadsafe(
                self._server.stop(), self._loop
            )
            try:
                fut.result(timeout=2.0)
            except Exception:
                pass
            self._loop.call_soon_threadsafe(self._loop.stop)
        if self._thread is not None:
            self._thread.join(timeout=2.0)


@pytest.fixture
def ipc_harness(monkeypatch):
    sock_path = _short_sock_path("mavlink.sock")
    harness = _IpcHarness(sock_path)
    harness.start()
    # Point the route's module-level MAVLINK_SOCK at our temp path so the
    # short-lived IPC client connects to the harness rather than the
    # production /run/ados socket which does not exist in CI.
    from ados.api.routes.ground_station import camera as cam_mod

    monkeypatch.setattr(cam_mod, "MAVLINK_SOCK", sock_path)
    try:
        yield harness
    finally:
        harness.stop()


def _patch_camera_count(monkeypatch, count: int) -> None:
    from ados.api.routes import ground_station as gs

    monkeypatch.setattr(gs, "_paired_drone_camera_count", lambda: count)


def test_single_camera_returns_501(client, monkeypatch):
    """Default placeholder (1 camera) must return 501 with the hint."""
    _patch_camera_count(monkeypatch, 1)
    resp = client.post(SWITCH_PATH, json={"camera_id": "2"})
    assert resp.status_code == 501
    body = resp.json()
    assert body["detail"] == "drone does not advertise multi-camera support"


def test_multi_camera_accepts_and_dispatches(client, monkeypatch, ipc_harness):
    """Two-camera drone: 200 + COMMAND_LONG (cmd 534) reaches IPC server."""
    _patch_camera_count(monkeypatch, 2)
    resp = client.post(SWITCH_PATH, json={"camera_id": "2"})
    assert resp.status_code == 200, resp.text
    body = resp.json()
    assert body["camera_id"] == "2"
    assert body["accepted"] is True
    assert body["reason"] is None

    # Wait briefly for the IPC server thread to record the uplink.
    deadline = asyncio.get_event_loop_policy().new_event_loop()
    deadline.close()
    for _ in range(40):
        if ipc_harness.received:
            break
        import time

        time.sleep(0.05)

    assert ipc_harness.received, "IPC harness did not receive any frame"
    raw = ipc_harness.received[-1]
    # The IPC server hands the inner MAVLink frame to the command handler
    # (length prefix already stripped). Parse with pymavlink to confirm
    # cmd 534 with param2 == 2.
    parser = mavlink2.MAVLink(None)
    parser.robust_parsing = True
    msg = parser.decode(bytearray(raw))
    assert msg is not None
    assert msg.get_msgId() == mavlink2.MAVLINK_MSG_ID_COMMAND_LONG
    assert msg.command == mavlink2.MAV_CMD_SET_CAMERA_SOURCE
    assert int(msg.param2) == 2


def test_drone_profile_rejected(drone_client, monkeypatch):
    """Drone-profile callers get 404 with the profile mismatch code."""
    _patch_camera_count(monkeypatch, 4)
    resp = drone_client.post(SWITCH_PATH, json={"camera_id": "2"})
    assert resp.status_code == 404
    assert resp.json()["detail"]["error"]["code"] == "E_PROFILE_MISMATCH"


def test_invalid_camera_id_returns_400(client, monkeypatch):
    """Non-numeric or out-of-range ids fail validation with 400."""
    _patch_camera_count(monkeypatch, 2)

    # Out-of-range numeric id.
    resp = client.post(SWITCH_PATH, json={"camera_id": "9"})
    assert resp.status_code == 400
    assert resp.json()["detail"]["error"]["code"] == "E_INVALID_CAMERA_ID"

    # Non-numeric id (no resolver mapping yet).
    resp = client.post(SWITCH_PATH, json={"camera_id": "thermal"})
    assert resp.status_code == 400


def test_ipc_unavailable_returns_503(client, monkeypatch):
    """If the local MAVLink IPC bus is down, return 503 so GCS retries."""
    _patch_camera_count(monkeypatch, 2)

    # Point at a path that does not exist so the IPC client can't connect.
    from ados.api.routes.ground_station import camera as cam_mod

    monkeypatch.setattr(
        cam_mod, "MAVLINK_SOCK", Path("/tmp/ados-camera-no-such-socket.sock")
    )
    resp = client.post(SWITCH_PATH, json={"camera_id": "2"})
    assert resp.status_code == 503
    assert resp.json()["detail"]["error"]["code"] == "E_MAVLINK_IPC_UNAVAILABLE"


def test_command_long_payload_round_trip():
    """Encoder/decoder agree on cmd 534 with the requested source index.

    Pure unit-test of the byte builder. Catches future encoder
    parameter regressions without standing up the IPC harness.
    """
    from ados.api.routes.ground_station.camera import (
        _build_set_camera_source_bytes,
    )

    # Length-prefix-style raw bytes (MAVLink frame, no IPC header).
    raw = _build_set_camera_source_bytes(3)
    # First byte is MAVLink v2 magic.
    assert raw[0] == 0xFD

    parser = mavlink2.MAVLink(None)
    parser.robust_parsing = True
    msg = parser.decode(bytearray(raw))
    assert msg is not None
    assert msg.command == mavlink2.MAV_CMD_SET_CAMERA_SOURCE
    assert int(msg.param2) == 3


def _ipc_payload_unwrap(framed: bytes) -> bytes:
    """Strip the 4-byte length prefix the IPC layer adds."""
    if len(framed) < 4:
        return framed
    (length,) = struct.unpack("!I", framed[:4])
    return framed[4 : 4 + length]
