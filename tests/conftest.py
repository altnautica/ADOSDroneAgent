"""Shared test fixtures."""

from __future__ import annotations

from unittest.mock import MagicMock

import pytest

from ados.core.config import ADOSConfig, load_config
from ados.services.mavlink.state import VehicleState


@pytest.fixture
def default_config() -> ADOSConfig:
    """A default ADOSConfig with no file loaded."""
    return ADOSConfig()


@pytest.fixture
def vehicle_state() -> VehicleState:
    """A fresh VehicleState instance."""
    return VehicleState()


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
