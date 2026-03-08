"""Tests for WFB-ng process manager."""

from __future__ import annotations

import asyncio
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from ados.core.config import WfbConfig
from ados.services.wfb.manager import LinkState, WfbManager


@pytest.fixture
def wfb_config() -> WfbConfig:
    return WfbConfig(interface="wlan0", channel=149, tx_power=25, fec_k=8, fec_n=12)


@pytest.fixture
def manager(wfb_config: WfbConfig) -> WfbManager:
    return WfbManager(wfb_config)


def test_initial_state(manager: WfbManager):
    assert manager.state == LinkState.DISCONNECTED
    assert manager.interface == ""
    assert manager.channel == 149


def test_link_state_enum():
    assert LinkState.DISCONNECTED == "disconnected"
    assert LinkState.CONNECTING == "connecting"
    assert LinkState.CONNECTED == "connected"
    assert LinkState.DEGRADED == "degraded"


def test_get_status(manager: WfbManager):
    status = manager.get_status()
    assert status["state"] == "disconnected"
    assert status["channel"] == 149
    assert status["rssi_dbm"] == -100.0
    assert status["packets_received"] == 0
    assert status["restart_count"] == 0
    assert "loss_percent" in status
    assert "samples" in status


@pytest.mark.asyncio
async def test_start_tx_not_found(manager: WfbManager):
    with patch("ados.services.wfb.manager.get_key_paths", return_value=("/tx.key", "/rx.key")):
        result = await manager.start_tx("wlan0", 149)
    assert result is False


@pytest.mark.asyncio
async def test_start_rx_not_found(manager: WfbManager):
    with patch("ados.services.wfb.manager.get_key_paths", return_value=("/tx.key", "/rx.key")):
        result = await manager.start_rx("wlan0", 149)
    assert result is False


@pytest.mark.asyncio
async def test_stop_no_processes(manager: WfbManager):
    # Should not raise
    await manager.stop()
    assert manager.state == LinkState.DISCONNECTED


@pytest.mark.asyncio
async def test_stop_with_processes(manager: WfbManager):
    mock_proc = AsyncMock()
    mock_proc.returncode = None
    mock_proc.terminate = MagicMock()
    mock_proc.wait = AsyncMock(return_value=0)
    mock_proc.pid = 12345

    manager._tx_proc = mock_proc
    manager._rx_proc = mock_proc
    manager._running = True

    await manager.stop()
    assert manager.state == LinkState.DISCONNECTED
    assert manager._tx_proc is None
    assert manager._rx_proc is None


def test_update_state_from_stats_connected(manager: WfbManager):
    from ados.services.wfb.link_quality import LinkStats
    stats = LinkStats(rssi_dbm=-55.0, loss_percent=1.0, packets_received=100)
    manager._update_state_from_stats(stats)
    assert manager.state == LinkState.CONNECTED


def test_update_state_from_stats_degraded_rssi(manager: WfbManager):
    from ados.services.wfb.link_quality import LinkStats
    stats = LinkStats(rssi_dbm=-90.0, loss_percent=1.0, packets_received=100)
    manager._update_state_from_stats(stats)
    assert manager.state == LinkState.DEGRADED


def test_update_state_from_stats_degraded_loss(manager: WfbManager):
    from ados.services.wfb.link_quality import LinkStats
    stats = LinkStats(rssi_dbm=-55.0, loss_percent=60.0, packets_received=100)
    manager._update_state_from_stats(stats)
    assert manager.state == LinkState.DEGRADED


def test_update_state_connecting(manager: WfbManager):
    from ados.services.wfb.link_quality import LinkStats
    stats = LinkStats(rssi_dbm=-55.0, loss_percent=0.0, packets_received=0)
    manager._update_state_from_stats(stats)
    assert manager.state == LinkState.CONNECTING
