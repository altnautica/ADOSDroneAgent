"""Tests for the text command listener (UDP + WebSocket)."""

from __future__ import annotations

from unittest.mock import AsyncMock, MagicMock

import pytest

from ados.core.config import TextCommandsConfig
from ados.services.scripting.text_listener import TextCommandListener, _UdpProtocol


@pytest.fixture
def mock_executor():
    ex = MagicMock()
    ex.execute = AsyncMock(return_value="ok")
    return ex


@pytest.fixture
def config() -> TextCommandsConfig:
    return TextCommandsConfig(udp_port=19889, websocket_port=19890)


class TestUdpProtocol:
    """UDP protocol handler for text commands."""

    def test_protocol_creation(self, mock_executor):
        proto = _UdpProtocol(mock_executor)
        assert proto._executor is mock_executor

    @pytest.mark.asyncio
    async def test_connection_made(self, mock_executor):
        proto = _UdpProtocol(mock_executor)
        transport = MagicMock()
        proto.connection_made(transport)
        assert proto._transport is transport

    @pytest.mark.asyncio
    async def test_handle_sends_response(self, mock_executor):
        proto = _UdpProtocol(mock_executor)
        transport = MagicMock()
        proto._transport = transport
        await proto._handle("takeoff", ("127.0.0.1", 12345))
        mock_executor.execute.assert_called_once()
        transport.sendto.assert_called_once()
        sent_data = transport.sendto.call_args[0][0]
        assert sent_data == b"ok"

    @pytest.mark.asyncio
    async def test_handle_empty_command(self, mock_executor):
        proto = _UdpProtocol(mock_executor)
        proto._transport = MagicMock()
        # Empty data should be ignored in datagram_received, but _handle still works
        await proto._handle("forward 100", ("127.0.0.1", 12345))
        mock_executor.execute.assert_called_once()


class TestTextCommandListener:
    """Listener construction and configuration."""

    def test_listener_creation(self, config: TextCommandsConfig, mock_executor):
        listener = TextCommandListener(config, mock_executor)
        assert listener._config.udp_port == 19889
        assert listener._config.websocket_port == 19890
