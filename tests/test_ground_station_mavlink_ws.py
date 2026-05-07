"""Tests for the ground-station MAVLink WebSocket bridge.

Covers:
* Profile gate: drone profile rejected.
* Downlink: frames published on the IPC socket reach the WebSocket peer
  as raw MAVLink bytes (no length prefix).
* Uplink: bytes sent on the WebSocket reach the IPC server's command
  handler unchanged.
* Disconnect: client hang-up tears down all bridge tasks cleanly.
"""

from __future__ import annotations

import asyncio
import tempfile
from pathlib import Path

import pytest
from fastapi.testclient import TestClient

from ados.api.server import create_app
from ados.core.config import ADOSConfig
from ados.core.ipc import MavlinkIPCServer
from tests.api_runtime_utils import build_api_runtime


def _short_sock_path(name: str) -> Path:
    """Return a short Unix socket path that fits in AF_UNIX's ~104-char limit.

    The macOS pytest tmp_path can exceed the limit, so route the socket
    through a short directory under the system temp root.
    """
    base = Path(tempfile.mkdtemp(prefix="ados-ws-"))
    return base / name


GS_PREFIX = "/api/v1/ground-station"
WS_PATH = f"{GS_PREFIX}/ws/mavlink"


# A valid MAVLink v2 HEARTBEAT (msgid 0) frame, 21 bytes total. The
# bridge does not parse MAVLink. It treats each frame as opaque bytes,
# so any non-empty payload exercises the round-trip. Using a real
# heartbeat keeps the test honest about the wire shape Android consumes.
HEARTBEAT_V2 = bytes(
    [
        0xFD,  # magic v2
        0x09,  # payload length
        0x00,  # incompat flags
        0x00,  # compat flags
        0x00,  # seq
        0x01,  # sysid
        0x01,  # compid
        0x00, 0x00, 0x00,  # msgid 0 (HEARTBEAT)
        # payload (9 bytes): custom_mode(4)=0, type(1)=2 quad,
        # autopilot(1)=3 ardupilot, base_mode(1)=0, system_status(1)=4 active,
        # mavlink_version(1)=3
        0x00, 0x00, 0x00, 0x00,
        0x02,
        0x03,
        0x00,
        0x04,
        0x03,
        # checksum (2 bytes). The bridge does not validate it; anything is fine.
        0x12, 0x34,
    ]
)


def _build_client(profile: str) -> TestClient:
    cfg = ADOSConfig()
    cfg.agent.profile = profile
    runtime = build_api_runtime(config=cfg)
    return TestClient(create_app(runtime))


@pytest.fixture
def drone_client() -> TestClient:
    return _build_client("auto")


class _IpcHarness:
    """Spin up a real MavlinkIPCServer on a tempfile path in a thread.

    The server is the same code the agent runs in production, so the
    test exercises the actual length-prefix framing the bridge unwraps.
    """

    def __init__(self, sock_path: Path) -> None:
        self.sock_path = sock_path
        self._server: MavlinkIPCServer | None = None
        self._loop: asyncio.AbstractEventLoop | None = None
        self._thread = None
        self._ready = asyncio.Event()
        self.received_uplink: list[bytes] = []

    def start(self) -> None:
        import threading

        ready = threading.Event()

        def _run() -> None:
            self._loop = asyncio.new_event_loop()
            asyncio.set_event_loop(self._loop)
            self._server = MavlinkIPCServer(sock_path=self.sock_path)
            self._server.set_command_handler(self.received_uplink.append)
            self._loop.run_until_complete(self._server.start())
            ready.set()
            self._loop.run_forever()

        self._thread = threading.Thread(target=_run, daemon=True)
        self._thread.start()
        ready.wait(timeout=5.0)

    def broadcast(self, frame: bytes) -> None:
        assert self._loop is not None
        assert self._server is not None
        srv = self._server
        loop = self._loop
        loop.call_soon_threadsafe(srv.broadcast, frame)

    def stop(self) -> None:
        if self._loop is None or self._server is None:
            return
        srv = self._server
        loop = self._loop
        fut = asyncio.run_coroutine_threadsafe(srv.stop(), loop)
        try:
            fut.result(timeout=5.0)
        except Exception:
            pass
        loop.call_soon_threadsafe(loop.stop)
        if self._thread is not None:
            self._thread.join(timeout=5.0)


@pytest.fixture
def ipc_harness(monkeypatch):
    sock = _short_sock_path("mavlink.sock")
    harness = _IpcHarness(sock)
    harness.start()

    # Point the bridge at our tempfile socket. The route module imports
    # MAVLINK_SOCK by name, so monkeypatching the attribute on the route
    # module is sufficient.
    from ados.api.routes.ground_station import mavlink_ws as ws_mod

    monkeypatch.setattr(ws_mod, "MAVLINK_SOCK", sock)

    yield harness
    harness.stop()


def test_ws_profile_gate_drone_rejected(drone_client: TestClient) -> None:
    """Drone profile must be refused with a 1008 close before any frames flow."""
    with pytest.raises(Exception):
        with drone_client.websocket_connect(WS_PATH):
            pass


def test_ws_downlink_forwards_raw_mavlink(ipc_harness: _IpcHarness) -> None:
    """A frame broadcast on the IPC bus arrives at the WebSocket as raw bytes."""
    client = _build_client("ground_station")
    with client.websocket_connect(WS_PATH) as ws:
        # Wait briefly for the bridge's IPC client to register on the
        # server side. Polling on client_count avoids a fixed sleep.
        for _ in range(50):
            assert ipc_harness._server is not None
            if ipc_harness._server.client_count >= 1:
                break
            import time as _t
            _t.sleep(0.05)
        else:
            pytest.fail("bridge IPC client never connected to server")

        ipc_harness.broadcast(HEARTBEAT_V2)
        received = ws.receive_bytes()
        assert received == HEARTBEAT_V2, (
            "downlink frame must arrive as raw MAVLink bytes, "
            "not the length-prefixed IPC framing"
        )


def test_ws_uplink_forwards_to_ipc_command_handler(ipc_harness: _IpcHarness) -> None:
    """Bytes the client sends on the WS reach the IPC command handler."""
    client = _build_client("ground_station")
    with client.websocket_connect(WS_PATH) as ws:
        # Same registration wait as downlink test.
        for _ in range(50):
            assert ipc_harness._server is not None
            if ipc_harness._server.client_count >= 1:
                break
            import time as _t
            _t.sleep(0.05)
        else:
            pytest.fail("bridge IPC client never connected to server")

        ws.send_bytes(HEARTBEAT_V2)

        # Command handler runs on the harness thread's loop. Poll until
        # it observes the frame or the test times out.
        for _ in range(50):
            if ipc_harness.received_uplink:
                break
            import time as _t
            _t.sleep(0.05)

        assert ipc_harness.received_uplink, "uplink frame never reached IPC handler"
        assert ipc_harness.received_uplink[0] == HEARTBEAT_V2


def test_ws_client_disconnect_releases_ipc(ipc_harness: _IpcHarness) -> None:
    """When the client closes, the bridge's IPC client drops off the server."""
    client = _build_client("ground_station")
    with client.websocket_connect(WS_PATH) as ws:
        for _ in range(50):
            assert ipc_harness._server is not None
            if ipc_harness._server.client_count >= 1:
                break
            import time as _t
            _t.sleep(0.05)
        # Confirm we got at least one bridge connected.
        assert ipc_harness._server is not None
        assert ipc_harness._server.client_count >= 1
        ws.close()

    # After the WS context exits, the bridge tasks tear down and the
    # IPC client disconnects. The server-side count drops back to zero.
    for _ in range(50):
        assert ipc_harness._server is not None
        if ipc_harness._server.client_count == 0:
            break
        import time as _t
        _t.sleep(0.05)
    assert ipc_harness._server is not None
    assert ipc_harness._server.client_count == 0, (
        "bridge did not release its IPC client on WS close"
    )


def test_ws_ipc_unavailable_closes_session(monkeypatch) -> None:
    """If the IPC socket is missing, the WS closes gracefully (no crash)."""
    missing = _short_sock_path("missing.sock")
    from ados.api.routes.ground_station import mavlink_ws as ws_mod

    # Speed up the failure path so the test does not wait the default
    # retry budget of 3 attempts at 0.5s each. The bridge's connect
    # call is the only consumer of these constants in tests.
    monkeypatch.setattr(ws_mod, "MAVLINK_SOCK", missing)

    client = _build_client("ground_station")
    # The bridge accepts the WebSocket, fails to connect to the IPC
    # socket after retries, and closes with code 1011. The next
    # receive attempt on the test client raises WebSocketDisconnect.
    with pytest.raises(Exception):
        with client.websocket_connect(WS_PATH) as ws:
            # First receive will surface the close; if the bridge hangs
            # instead, the harness times out and the test fails.
            ws.receive_bytes()
