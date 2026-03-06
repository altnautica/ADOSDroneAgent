"""Tests for WebSocket proxy relay."""

from __future__ import annotations

import asyncio
from unittest.mock import AsyncMock, MagicMock

import pytest

from ados.core.config import MavlinkConfig
from ados.services.mavlink.proxy import MavlinkProxy


@pytest.fixture
def mock_fc():
    """Mock FCConnection with subscriber queue."""
    fc = MagicMock()
    q = asyncio.Queue()
    fc.subscribe.return_value = q
    fc.unsubscribe = MagicMock()
    fc.send_bytes = MagicMock()
    return fc, q


def test_proxy_init(mock_fc):
    """MavlinkProxy should initialize with default port."""
    fc, _ = mock_fc
    config = MavlinkConfig()
    proxy = MavlinkProxy(config, fc)
    assert proxy._port == 8765


def test_proxy_custom_port(mock_fc):
    """MavlinkProxy should use config endpoint port."""
    from ados.core.config import EndpointConfig
    fc, _ = mock_fc
    config = MavlinkConfig(endpoints=[
        EndpointConfig(type="websocket", port=9999, enabled=True),
    ])
    proxy = MavlinkProxy(config, fc)
    assert proxy._port == 9999


@pytest.mark.asyncio
async def test_proxy_broadcast(mock_fc):
    """FC data should be broadcast to connected clients."""
    fc, q = mock_fc
    config = MavlinkConfig()
    proxy = MavlinkProxy(config, fc)

    # Simulate a connected client
    mock_ws = AsyncMock()
    mock_ws.remote_address = ("127.0.0.1", 12345)
    proxy._clients.add(mock_ws)

    # Put data in the FC queue
    test_data = b"\xfd\x09\x00\x00\x00\x01\x01"
    await q.put(test_data)

    # Run broadcast once
    broadcast_task = asyncio.create_task(proxy._broadcast_fc_data())
    await asyncio.sleep(0.1)
    broadcast_task.cancel()

    mock_ws.send.assert_called_with(test_data)


@pytest.mark.asyncio
async def test_proxy_client_to_fc(mock_fc):
    """Client bytes should be forwarded to FC."""
    fc, _ = mock_fc
    config = MavlinkConfig()
    proxy = MavlinkProxy(config, fc)

    # Simulate incoming client data
    test_data = b"\xfd\x00\x01"

    # Mock WebSocket with proper async iterator
    class FakeWs:
        remote_address = ("127.0.0.1", 54321)
        def __aiter__(self):
            return self
        def __init__(self):
            self._items = [test_data]
            self._idx = 0
        async def __anext__(self):
            if self._idx >= len(self._items):
                raise StopAsyncIteration
            val = self._items[self._idx]
            self._idx += 1
            return val

    mock_ws = FakeWs()
    await proxy._handle_client(mock_ws)
    fc.send_bytes.assert_called_with(test_data)
