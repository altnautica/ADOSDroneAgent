"""Shared test fixtures."""

from __future__ import annotations

import shutil
import tempfile
from collections.abc import Iterator
from pathlib import Path
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

    And two node-level paths the config machinery owns, for the same reason:

    * ``ados.core.config._lock.CONFIG_LOCK`` — the reader/writer flock. A
      test that took the real one would serialise against a live agent on
      the developer's own box.
    * ``ados.core.config.maintenance.CONFIG_MIGRATIONS_PATH`` — the
      one-shot-cleanup ledger. This one bites hardest: a test run that
      recorded a cleanup in the real ledger made a *later* test in the same
      suite pass or fail depending on whether an earlier run had happened,
      which is exactly the order-dependence this fixture exists to kill.
    """
    import ados.api.deps as deps
    import ados.core.profile as profile
    from ados.core.config import _lock
    from ados.core.config import maintenance as config_maintenance

    # Force the singleton empty around each test (raw, not monkeypatch-restore,
    # so a value leaked by a prior test can never be restored back).
    deps._agent_app = None
    # Point the profile-conf reader at a fresh per-test file (monkeypatch
    # restores the real path afterward).
    monkeypatch.setattr(profile, "PROFILE_CONF", tmp_path / "profile.conf")
    monkeypatch.setattr(_lock, "CONFIG_LOCK", tmp_path / "config.yaml.lock")
    monkeypatch.setattr(
        config_maintenance,
        "CONFIG_MIGRATIONS_PATH",
        tmp_path / "config-migrations.json",
    )
    yield
    deps._agent_app = None


def _short_tmp_root() -> str:
    """The shortest writable temp root available, for AF_UNIX paths.

    A unix socket address is capped at ``sizeof(sun_path)`` — 104 bytes on
    macOS/BSD, 108 on Linux — and the kernel counts the literal string, not the
    resolved path. pytest's ``tmp_path`` on macOS is ~122 characters
    (``/private/var/folders/<2>/<28>/T/pytest-of-<user>/pytest-<n>/<testname>0``)
    before a filename is appended, so any test binding a socket under it fails
    with ``OSError: AF_UNIX path too long``. ``/tmp`` keeps the whole address
    around 30 bytes on both platforms.
    """
    for candidate in ("/tmp", tempfile.gettempdir()):
        probe = Path(candidate)
        if probe.is_dir():
            return candidate
    return tempfile.gettempdir()


@pytest.fixture
def unix_socket_dir() -> Iterator[Path]:
    """A per-test directory short enough to hold an AF_UNIX socket path.

    Use this instead of ``tmp_path`` for any socket a test actually binds. The
    directory is removed afterwards, sockets and all.
    """
    path = Path(tempfile.mkdtemp(prefix="ados-s", dir=_short_tmp_root()))
    try:
        yield path
    finally:
        shutil.rmtree(path, ignore_errors=True)


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
