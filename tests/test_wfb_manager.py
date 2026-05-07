"""Tests for WFB-ng process manager."""

from __future__ import annotations

from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from ados.core.config import WfbConfig
from ados.services.wfb.manager import LinkState, WfbManager


@pytest.fixture
def wfb_config() -> WfbConfig:
    return WfbConfig(interface="wlan0", channel=149, tx_power_dbm=5, fec_k=8, fec_n=12)


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


def test_wfb_config_legacy_tx_power_dropped():
    """Old YAML field `tx_power` was MCS-index in disguise; drop it on load."""
    cfg = WfbConfig.model_validate({"tx_power": 25})
    assert cfg.tx_power_dbm == 5
    assert cfg.mcs_index == 1
    assert cfg.topology == "host_vbus"


def test_wfb_config_clamps_to_ceiling():
    cfg = WfbConfig(tx_power_dbm=99)
    assert cfg.tx_power_dbm == cfg.tx_power_max_dbm == 15


def test_wfb_config_clamps_to_floor():
    cfg = WfbConfig(tx_power_dbm=0)
    assert cfg.tx_power_dbm == 1


def test_get_status_carries_tx_power(manager: WfbManager):
    status = manager.get_status()
    assert status["tx_power_dbm"] is None  # never applied yet
    assert status["tx_power_max_dbm"] == 15
    assert status["topology"] == "host_vbus"
    assert status["mcs_index"] == 1


def test_apply_tx_power_no_interface(manager: WfbManager):
    assert manager.apply_tx_power(5) is None


def test_apply_tx_power_clamps_above_ceiling(manager: WfbManager):
    manager._interface = "wlan1"
    with patch("ados.services.wfb.manager.set_tx_power", return_value=15) as mock_set:
        effective = manager.apply_tx_power(99)
    assert effective == 15
    mock_set.assert_called_once_with("wlan1", 15)
    assert manager.effective_tx_power_dbm == 15


def test_apply_tx_power_clamps_below_floor(manager: WfbManager):
    manager._interface = "wlan1"
    with patch("ados.services.wfb.manager.set_tx_power", return_value=1) as mock_set:
        effective = manager.apply_tx_power(0)
    assert effective == 1
    mock_set.assert_called_once_with("wlan1", 1)


def test_apply_tx_power_returns_none_on_driver_reject(manager: WfbManager):
    manager._interface = "wlan1"
    with patch("ados.services.wfb.manager.set_tx_power", return_value=None):
        effective = manager.apply_tx_power(5)
    assert effective is None
    assert manager.effective_tx_power_dbm is None
