"""Tests for WireGuard tunnel management."""

from __future__ import annotations

from unittest.mock import MagicMock, patch

import pytest

from ados.core.config import WireguardConfig
from ados.security.wireguard import WireguardManager


@pytest.fixture
def wg_config():
    return WireguardConfig(enabled=True)


@pytest.fixture
def wg_manager(wg_config):
    return WireguardManager(wg_config)


def test_start_tunnel_disabled():
    config = WireguardConfig(enabled=False)
    mgr = WireguardManager(config)
    assert mgr.start_tunnel() is False


def test_start_tunnel_not_linux(wg_manager):
    with patch("ados.security.wireguard.platform.system", return_value="Darwin"):
        result = wg_manager.start_tunnel()
    assert result is False


def test_start_tunnel_linux_success(wg_manager):
    with patch("ados.security.wireguard.platform.system", return_value="Linux"), \
         patch("subprocess.run") as mock_run:
        mock_run.return_value = MagicMock(returncode=0, stdout="", stderr="")
        result = wg_manager.start_tunnel()
    assert result is True


def test_stop_tunnel_linux(wg_manager):
    with patch("ados.security.wireguard.platform.system", return_value="Linux"), \
         patch("subprocess.run") as mock_run:
        mock_run.return_value = MagicMock(returncode=0, stdout="", stderr="")
        result = wg_manager.stop_tunnel()
    assert result is True


def test_is_active_linux(wg_manager):
    with patch("ados.security.wireguard.platform.system", return_value="Linux"), \
         patch("subprocess.run") as mock_run:
        mock_run.return_value = MagicMock(returncode=0, stdout="interface: ados", stderr="")
        assert wg_manager.is_active() is True


def test_is_active_not_running(wg_manager):
    with patch("ados.security.wireguard.platform.system", return_value="Linux"), \
         patch("subprocess.run") as mock_run:
        mock_run.return_value = MagicMock(returncode=1, stdout="", stderr="not found")
        assert wg_manager.is_active() is False


def test_get_status_not_linux(wg_manager):
    with patch("ados.security.wireguard.platform.system", return_value="Darwin"):
        status = wg_manager.get_status()
    assert status["active"] is False
    assert status["reason"] == "not_linux"


def test_get_status_linux_active(wg_manager):
    wg_output = (
        "interface: ados\n"
        "  public key: abc123\n"
        "  transfer: 1.2 GiB received, 500 MiB sent\n"
        "  latest handshake: 30 seconds ago\n"
        "  endpoint: 10.0.0.1:51820\n"
    )
    with patch("ados.security.wireguard.platform.system", return_value="Linux"), \
         patch("subprocess.run") as mock_run:
        mock_run.return_value = MagicMock(returncode=0, stdout=wg_output, stderr="")
        status = wg_manager.get_status()

    assert status["active"] is True
    assert status["interface"] == "ados"
    assert "public_key" in status


def test_generate_keypair_not_linux(wg_manager):
    with patch("ados.security.wireguard.platform.system", return_value="Darwin"):
        priv, pub = wg_manager.generate_keypair()
    assert priv == ""
    assert pub == ""


def test_command_binary_not_found(wg_manager):

    with patch("ados.security.wireguard.platform.system", return_value="Linux"), \
         patch("subprocess.run", side_effect=FileNotFoundError):
        result = wg_manager.start_tunnel()
    assert result is False
