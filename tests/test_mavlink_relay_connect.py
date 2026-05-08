"""Tests for MQTT connect failure propagation in ``MavlinkMqttRelay.start``.

A bare ``except Exception`` previously swallowed the connect error and
left the relay alive but disconnected forever. The contract now is to
re-raise so systemd notices the failure and restarts the unit.
"""

from __future__ import annotations

import asyncio
from unittest import mock

import pytest

from ados.services.cloud.mavlink_relay import MavlinkMqttRelay


async def test_start_propagates_connect_oserror() -> None:
    relay = MavlinkMqttRelay(
        device_id="test-dev",
        broker="example.invalid",
        port=8883,
        transport="websockets",
        username="u",
        password="p",
    )

    shutdown = asyncio.Event()

    # Patch paho Client at the module level so the constructor returns
    # a mock with a connect() that raises OSError.
    with mock.patch(
        "ados.services.cloud.mavlink_relay.mqtt_client.Client"
    ) as mock_client_cls:
        client = mock.MagicMock()
        client.connect.side_effect = OSError("nameserver lookup failed")
        mock_client_cls.return_value = client

        with pytest.raises(OSError, match="nameserver lookup failed"):
            await relay.start(shutdown)


async def test_start_propagates_connection_refused() -> None:
    relay = MavlinkMqttRelay(
        device_id="test-dev-2",
        broker="127.0.0.1",
        port=1883,
        transport="tcp",
    )

    shutdown = asyncio.Event()

    with mock.patch(
        "ados.services.cloud.mavlink_relay.mqtt_client.Client"
    ) as mock_client_cls:
        client = mock.MagicMock()
        client.connect.side_effect = ConnectionRefusedError("nope")
        mock_client_cls.return_value = client

        with pytest.raises(ConnectionRefusedError):
            await relay.start(shutdown)
