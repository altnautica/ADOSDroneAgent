"""Tests for POST /api/command — FC-status gate + MAVLink IPC dispatch.

In the multi-process runtime the REST API has no in-process pymavlink
connection (the native router owns the FC serial link), so the command route
must gate on the same fc-connected signal `/api/status` reports
(`app.fc_status().connected`) and write the COMMAND_LONG frame to the MAVLink
IPC socket the router reads.

Covers:
* 503 when the FC is not connected.
* 200 + a real COMMAND_LONG reaching a live MavlinkIPCServer when connected.
* 503 when the FC reports connected but the MAVLink socket is unreachable.
* 400 on an unknown command / a mode with no name.
"""

from __future__ import annotations

import asyncio
import threading
import time
from pathlib import Path
from tempfile import mkdtemp
from unittest.mock import MagicMock

import pytest
from fastapi.testclient import TestClient
from pymavlink.dialects.v20 import common as mavlink2

from ados.api.server import create_app
from ados.core.ipc import MavlinkIPCServer
from tests.api_runtime_utils import build_api_runtime


def _connected_fc() -> MagicMock:
    fc = MagicMock()
    fc.connected = True
    fc.port = "/dev/ttyACM0"
    fc.baud = 115200
    return fc


def _short_sock_path(name: str) -> Path:
    return Path(mkdtemp(prefix="ados-cmd-")) / name


class _IpcHarness:
    """Run a real MavlinkIPCServer in a thread + capture inbound command bytes."""

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
            fut = asyncio.run_coroutine_threadsafe(self._server.stop(), self._loop)
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
    # Point the route's MAVLINK_SOCK at the harness socket; the production
    # /run/ados path does not exist in CI.
    from ados.api.routes import commands as cmd_mod

    monkeypatch.setattr(cmd_mod, "MAVLINK_SOCK", sock_path)
    try:
        yield harness
    finally:
        harness.stop()


def test_command_no_fc_returns_503():
    """A runtime whose fc-status snapshot is disconnected returns 503."""
    runtime = build_api_runtime()  # default fc is disconnected
    client = TestClient(create_app(runtime))
    resp = client.post("/api/command", json={"cmd": "arm"})
    assert resp.status_code == 503
    assert resp.json()["detail"] == "FC not connected"


def test_command_connected_sends_frame_over_ipc(ipc_harness):
    """When the FC reports connected, the route builds + ships a COMMAND_LONG."""
    runtime = build_api_runtime(fc_connection=_connected_fc())
    client = TestClient(create_app(runtime))

    resp = client.post("/api/command", json={"cmd": "arm"})
    assert resp.status_code == 200, resp.text
    assert resp.json() == {"status": "ok", "cmd": "arm"}

    for _ in range(40):
        if ipc_harness.received:
            break
        time.sleep(0.05)
    assert ipc_harness.received, "IPC harness did not receive the command frame"

    # The handler is handed the inner MAVLink frame (length prefix stripped).
    parser = mavlink2.MAVLink(None)
    parser.robust_parsing = True
    msg = parser.decode(bytearray(ipc_harness.received[-1]))
    assert msg is not None
    assert msg.get_msgId() == mavlink2.MAVLINK_MSG_ID_COMMAND_LONG
    assert msg.command == mavlink2.MAV_CMD_COMPONENT_ARM_DISARM
    assert int(msg.param1) == 1  # arm
    assert msg.target_system == 1
    assert msg.target_component == 1


def test_takeoff_alt_rides_in_param7(ipc_harness):
    """takeoff with an altitude arg encodes it as param7 of NAV_TAKEOFF."""
    runtime = build_api_runtime(fc_connection=_connected_fc())
    client = TestClient(create_app(runtime))

    resp = client.post("/api/command", json={"cmd": "takeoff", "args": [25.0]})
    assert resp.status_code == 200, resp.text
    assert resp.json()["altitude"] == 25.0

    for _ in range(40):
        if ipc_harness.received:
            break
        time.sleep(0.05)
    assert ipc_harness.received

    parser = mavlink2.MAVLink(None)
    parser.robust_parsing = True
    msg = parser.decode(bytearray(ipc_harness.received[-1]))
    assert msg.command == mavlink2.MAV_CMD_NAV_TAKEOFF
    assert msg.param7 == pytest.approx(25.0)


def test_command_connected_but_ipc_down_returns_503(monkeypatch):
    """FC connected yet no MAVLink socket → 503, command never silently dropped."""
    runtime = build_api_runtime(fc_connection=_connected_fc())
    client = TestClient(create_app(runtime))

    from ados.api.routes import commands as cmd_mod

    monkeypatch.setattr(
        cmd_mod, "MAVLINK_SOCK", Path("/tmp/ados-command-no-such-socket.sock")
    )
    resp = client.post("/api/command", json={"cmd": "arm"})
    assert resp.status_code == 503
    assert resp.json()["detail"] == "No MAVLink connection"


def test_unknown_command_is_400(ipc_harness):
    runtime = build_api_runtime(fc_connection=_connected_fc())
    client = TestClient(create_app(runtime))
    resp = client.post("/api/command", json={"cmd": "fly-to-the-moon"})
    assert resp.status_code == 400
    assert "Unknown command" in resp.json()["detail"]


def test_mode_with_no_name_is_400(ipc_harness):
    runtime = build_api_runtime(fc_connection=_connected_fc())
    client = TestClient(create_app(runtime))
    resp = client.post("/api/command", json={"cmd": "mode"})
    assert resp.status_code == 400
    assert resp.json()["detail"] == "Mode name required"
