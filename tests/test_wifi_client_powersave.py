"""WiFi client manager forces power-save off after a successful join.

The station link must never park the radio under light traffic, which
silently drops the management/uplink connection. ``join`` runs an
nmcli connection-level toggle plus an iw runtime toggle right after the
connect succeeds; both are best-effort and a failure must not fail the
join.
"""

from __future__ import annotations

from unittest.mock import patch

import pytest

import ados.services.ground_station.wifi_client_manager as wcm
from ados.services.ground_station.wifi_client_manager import WifiClientManager


@pytest.fixture
def manager(monkeypatch) -> WifiClientManager:
    mgr = WifiClientManager(interface="wlan0")
    # Neutralize lock + hostapd + file IO so the join path runs the
    # command sequence without touching the real system.
    monkeypatch.setattr(mgr, "_acquire_lock", lambda: True)
    monkeypatch.setattr(mgr, "_release_lock", lambda: None)
    monkeypatch.setattr(mgr, "_is_hostapd_active", lambda: False)
    monkeypatch.setattr(mgr, "_write_ap_flag", lambda enabled: None)
    monkeypatch.setattr(mgr, "_load_client_config", lambda: {})
    monkeypatch.setattr(mgr, "_save_client_config", lambda data: None)

    async def _fake_status() -> dict:
        return {"ssid": "HomeWifi", "signal": 70, "ip": "192.168.1.50", "gateway": "192.168.1.1"}

    monkeypatch.setattr(mgr, "status", _fake_status)
    return mgr


@pytest.mark.asyncio
async def test_join_disables_powersave(manager):
    issued: list[list[str]] = []

    async def fake_run(cmd, timeout=15.0):
        issued.append(list(cmd))
        return 0, "", ""

    with patch.object(wcm, "_run", side_effect=fake_run):
        result = await manager.join("HomeWifi", "secret")

    assert result["joined"] is True
    # nmcli connect happened first.
    assert ["nmcli", "device", "wifi", "connect", "HomeWifi", "password", "secret",
            "ifname", "wlan0"] in issued
    # Connection-level power-save toggle keyed on the SSID (NM names the
    # new profile after the SSID) and the runtime iw toggle both fired.
    assert ["nmcli", "connection", "modify", "HomeWifi",
            "802-11-wireless.powersave", "2"] in issued
    assert ["iw", "dev", "wlan0", "set", "power_save", "off"] in issued

    # Power-save toggles must come AFTER the connect.
    connect_idx = issued.index(
        ["nmcli", "device", "wifi", "connect", "HomeWifi", "password", "secret",
         "ifname", "wlan0"]
    )
    ps_iw_idx = issued.index(["iw", "dev", "wlan0", "set", "power_save", "off"])
    assert ps_iw_idx > connect_idx


@pytest.mark.asyncio
async def test_join_powersave_failure_is_nonfatal(manager):
    """A power-save toggle failure must not fail an otherwise-good join."""

    async def fake_run(cmd, timeout=15.0):
        if "power_save" in cmd or "802-11-wireless.powersave" in cmd:
            return 1, "", "not supported"
        return 0, "", ""

    with patch.object(wcm, "_run", side_effect=fake_run):
        result = await manager.join("HomeWifi", "secret")

    assert result["joined"] is True


@pytest.mark.asyncio
async def test_failed_connect_skips_powersave(manager):
    """No power-save toggle is attempted when the connect itself fails."""
    issued: list[list[str]] = []

    async def fake_run(cmd, timeout=15.0):
        issued.append(list(cmd))
        if cmd[:4] == ["nmcli", "device", "wifi", "connect"]:
            return 1, "", "No network with SSID 'HomeWifi' found"
        return 0, "", ""

    with patch.object(wcm, "_run", side_effect=fake_run):
        result = await manager.join("HomeWifi", "secret")

    assert result["joined"] is False
    assert ["iw", "dev", "wlan0", "set", "power_save", "off"] not in issued
    assert ["nmcli", "connection", "modify", "HomeWifi",
            "802-11-wireless.powersave", "2"] not in issued
