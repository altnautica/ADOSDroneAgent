"""Tests for the supervisor heartbeat radio block."""

from __future__ import annotations

from ados.core.supervisor.heartbeat import (
    _channel_to_freq,
    build_radio_block,
)


def test_radio_block_absent_when_status_missing():
    """build_radio_block(None) returns an `absent` block with all-null fields."""
    block = build_radio_block(None)
    assert block["state"] == "absent"
    for key in (
        "iface",
        "driver",
        "channel",
        "freq_mhz",
        "tx_power_dbm",
        "tx_power_max_dbm",
        "topology",
        "rssi_dbm",
        "bitrate_kbps",
        "fec_recovered",
        "fec_lost",
        "packets_lost",
    ):
        assert block[key] is None, key


def test_radio_block_with_full_status():
    """build_radio_block from a manager dict maps to the wire schema."""
    status = {
        "state": "connected",
        "interface": "wlan1",
        "channel": 149,
        "rssi_dbm": -55.0,
        "bitrate_kbps": 8000,
        "fec_recovered": 12,
        "fec_failed": 3,
        "packets_lost": 7,
        "tx_power_dbm": 5,
        "tx_power_max_dbm": 15,
        "topology": "host_vbus",
        "mcs_index": 1,
    }
    block = build_radio_block(status)
    assert block["state"] == "connected"
    assert block["iface"] == "wlan1"
    assert block["channel"] == 149
    assert block["freq_mhz"] == 5745
    assert block["bandwidth_mhz"] == 20
    assert block["rssi_dbm"] == -55.0
    assert block["bitrate_kbps"] == 8000
    assert block["tx_power_dbm"] == 5
    assert block["tx_power_max_dbm"] == 15
    assert block["topology"] == "host_vbus"
    assert block["fec_recovered"] == 12
    assert block["fec_lost"] == 3
    assert block["packets_lost"] == 7


def test_radio_block_treats_sentinel_rssi_as_null():
    """RSSI seeded at -100 dBm before first sample is reported as None."""
    status = {
        "state": "connecting",
        "interface": "wlan0",
        "channel": 36,
        "rssi_dbm": -100.0,
        "bitrate_kbps": 0,
        "fec_recovered": 0,
        "fec_failed": 0,
        "packets_lost": 0,
        "tx_power_dbm": 1,
        "tx_power_max_dbm": 15,
        "topology": "host_vbus",
    }
    block = build_radio_block(status)
    assert block["rssi_dbm"] is None
    assert block["bitrate_kbps"] is None
    assert block["freq_mhz"] == 5180


def test_channel_to_freq_known_and_unknown():
    """_channel_to_freq handles known channels and bad input."""
    assert _channel_to_freq(149) == 5745
    assert _channel_to_freq(36) == 5180
    assert _channel_to_freq(999) is None
    assert _channel_to_freq(None) is None
    assert _channel_to_freq("abc") is None


def test_supervisor_heartbeat_payload_includes_radio_block():
    """Supervisor.get_heartbeat_payload() emits a `radio` key, sourced via fallback when no manager."""
    from collections import deque
    from unittest.mock import patch

    from ados.core.supervisor.lifecycle import Supervisor

    from ados.core.config import ADOSConfig
    sup = Supervisor(ADOSConfig())
    # Empty service registry is fine; the mixin still needs the deques.
    sup._cpu_history = deque(maxlen=10)
    sup._memory_history = deque(maxlen=10)
    sup._active_suite = None

    # No in-process WfbManager and the localhost fallback fails (no
    # agent running in the test process), so the block is `absent`.
    with patch(
        "ados.core.supervisor.heartbeat.fetch_wfb_status_via_http",
        return_value=None,
    ):
        payload = sup.get_heartbeat_payload()

    assert "radio" in payload
    assert payload["radio"]["state"] == "absent"


def test_supervisor_heartbeat_payload_uses_attached_manager():
    """When the supervisor has an attached manager, its status drives the radio block."""
    from collections import deque

    from ados.core.supervisor.lifecycle import Supervisor

    class FakeManager:
        def get_status(self):
            return {
                "state": "connected",
                "interface": "wlan1",
                "channel": 161,
                "rssi_dbm": -60.0,
                "bitrate_kbps": 12000,
                "fec_recovered": 4,
                "fec_failed": 1,
                "packets_lost": 2,
                "tx_power_dbm": 5,
                "tx_power_max_dbm": 15,
                "topology": "host_vbus",
            }

    from ados.core.config import ADOSConfig
    sup = Supervisor(ADOSConfig())
    sup._cpu_history = deque(maxlen=10)
    sup._memory_history = deque(maxlen=10)
    sup._active_suite = None
    sup._wfb_manager = FakeManager()

    payload = sup.get_heartbeat_payload()

    assert payload["radio"]["state"] == "connected"
    assert payload["radio"]["iface"] == "wlan1"
    assert payload["radio"]["channel"] == 161
    assert payload["radio"]["freq_mhz"] == 5805
    assert payload["radio"]["rssi_dbm"] == -60.0
    assert payload["radio"]["tx_power_dbm"] == 5
