"""Shared test fixtures."""

from __future__ import annotations

from unittest.mock import MagicMock

import pytest

from ados.core.config import ADOSConfig
from ados.services.mavlink.ipc_state import IpcVehicleState


@pytest.fixture(autouse=True)
def _isolate_agent_globals(tmp_path, monkeypatch):
    """Isolate the process-global state that leaks across tests.

    Two module-globals otherwise make test outcomes order-dependent:

    * ``ados.api.deps._agent_app`` — the agent-app singleton ``create_app`` /
      ``set_agent_app`` populate. Reset it so it never leaks between tests.
    * ``ados.core.profile.PROFILE_CONF`` — the profile gate resolves an
      ``auto`` node by reading ``/etc/ados/profile.conf``. Building an app in a
      test triggers first-boot profile detection that WRITES that real path
      (as ``ground_station`` on a generic board with no FC), which then flips
      every later ``auto`` client's resolved profile. Point the reader at a
      fresh per-test file so one test's write can never reach another.
    """
    import ados.api.deps as deps
    import ados.core.profile as profile

    # Force the singleton empty around each test (raw, not monkeypatch-restore,
    # so a value leaked by a prior test can never be restored back).
    deps._agent_app = None
    # Point the profile-conf reader at a fresh per-test file (monkeypatch
    # restores the real path afterward).
    monkeypatch.setattr(profile, "PROFILE_CONF", tmp_path / "profile.conf")
    yield
    deps._agent_app = None


@pytest.fixture
def default_config() -> ADOSConfig:
    """A default ADOSConfig with no file loaded."""
    return ADOSConfig()


@pytest.fixture
def vehicle_state() -> IpcVehicleState:
    """A fresh vehicle-state view backed by the router's state IPC snapshot."""
    return IpcVehicleState()


@pytest.fixture
def mock_mavlink_msg():
    """Factory for mock MAVLink messages."""
    def _make(msg_type: str, **fields):
        msg = MagicMock()
        msg.get_type.return_value = msg_type
        for k, v in fields.items():
            setattr(msg, k, v)
        msg.get_msgbuf.return_value = b"\xfd\x00\x00\x00"
        return msg
    return _make


@pytest.fixture
def mock_fc_connection():
    """A mock FCConnection."""
    import asyncio
    conn = MagicMock()
    conn.connected = True
    conn.port = "/dev/ttyACM0"
    conn.baud = 115200
    conn.connection = MagicMock()
    q = asyncio.Queue()
    conn.subscribe.return_value = q
    conn.send_bytes = MagicMock()
    return conn
