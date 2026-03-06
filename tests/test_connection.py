"""Tests for FC connection — serial detection, baud detection (mocked)."""

from __future__ import annotations

from unittest.mock import MagicMock, patch

from ados.services.mavlink.connection import auto_detect_port


@patch("glob.glob")
def test_auto_detect_port_acm(mock_glob):
    """Should find /dev/ttyACM0 first."""
    mock_glob.side_effect = lambda pattern: (
        ["/dev/ttyACM0"] if "ACM" in pattern else []
    )
    port = auto_detect_port()
    assert port == "/dev/ttyACM0"


@patch("glob.glob")
def test_auto_detect_port_usb(mock_glob):
    """Should find /dev/ttyUSB0 if no ACM/AMA."""
    def side_effect(pattern):
        if "USB" in pattern:
            return ["/dev/ttyUSB0"]
        return []
    mock_glob.side_effect = side_effect
    port = auto_detect_port()
    assert port == "/dev/ttyUSB0"


@patch("glob.glob")
def test_auto_detect_port_none(mock_glob):
    """Should return None if no serial ports found."""
    mock_glob.return_value = []
    port = auto_detect_port()
    assert port is None


def test_fc_connection_init():
    """FCConnection should initialize with config."""
    from ados.core.config import MavlinkConfig
    from ados.services.mavlink.state import VehicleState

    config = MavlinkConfig(serial_port="/dev/ttyACM0", baud_rate=115200)
    state = VehicleState()
    from ados.services.mavlink.connection import FCConnection
    fc = FCConnection(config, state)

    assert fc.connected is False
    assert fc.port == ""
    assert fc.baud == 0


def test_fc_connection_subscribe():
    """subscribe() should return an asyncio Queue."""
    import asyncio
    from ados.core.config import MavlinkConfig
    from ados.services.mavlink.state import VehicleState
    from ados.services.mavlink.connection import FCConnection

    config = MavlinkConfig()
    state = VehicleState()
    fc = FCConnection(config, state)

    q = fc.subscribe()
    assert isinstance(q, asyncio.Queue)
    assert len(fc._subscribers) == 1

    fc.unsubscribe(q)
    assert len(fc._subscribers) == 0
