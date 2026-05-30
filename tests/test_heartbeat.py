"""Tests for the heartbeat radio block builder."""

from __future__ import annotations

from ados.core.radio_block import (
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
        "snr_db",
        "noise_dbm",
        "bitrate_kbps",
        "fec_recovered",
        "fec_lost",
        "packets_lost",
        "loss_percent",
        "mcs_index",
        "rx_silent_seconds",
    ):
        assert block[key] is None, key


def test_radio_block_with_full_status():
    """build_radio_block from a manager dict maps to the wire schema."""
    status = {
        "state": "connected",
        "interface": "wlan1",
        "channel": 149,
        "rssi_dbm": -55.0,
        "snr_db": 28.0,
        "noise_dbm": -90.0,
        "bitrate_kbps": 8000,
        "fec_recovered": 12,
        "fec_failed": 3,
        "packets_lost": 7,
        "loss_percent": 1.5,
        "tx_power_dbm": 5,
        "tx_power_max_dbm": 15,
        "topology": "host_vbus",
        "mcs_index": 1,
        "rx_silent_seconds": 0.2,
    }
    block = build_radio_block(status)
    assert block["state"] == "connected"
    assert block["iface"] == "wlan1"
    assert block["channel"] == 149
    assert block["freq_mhz"] == 5745
    assert block["bandwidth_mhz"] == 20
    assert block["rssi_dbm"] == -55.0
    assert block["snr_db"] == 28.0
    assert block["noise_dbm"] == -90.0
    assert block["bitrate_kbps"] == 8000
    assert block["tx_power_dbm"] == 5
    assert block["tx_power_max_dbm"] == 15
    assert block["topology"] == "host_vbus"
    assert block["fec_recovered"] == 12
    assert block["fec_lost"] == 3
    assert block["packets_lost"] == 7
    assert block["loss_percent"] == 1.5
    assert block["mcs_index"] == 1
    assert block["rx_silent_seconds"] == 0.2


def test_radio_block_carries_adapter_verdict():
    """The selected adapter chipset + injection verdict ride the block."""
    status = {
        "state": "connected",
        "interface": "wlan1",
        "channel": 149,
        "adapter_chipset": "RTL8812EU",
        "adapter_injection_ok": True,
    }
    block = build_radio_block(status)
    assert block["adapter_chipset"] == "RTL8812EU"
    assert block["adapter_injection_ok"] is True


def test_radio_block_no_injection_adapter_is_loud():
    """No injection-capable adapter → chipset null, injection_ok false."""
    status = {
        "state": "disconnected",
        "interface": "",
        "channel": 149,
        "adapter_chipset": None,
        "adapter_injection_ok": False,
    }
    block = build_radio_block(status)
    assert block["adapter_chipset"] is None
    assert block["adapter_injection_ok"] is False


def test_radio_block_absent_defaults_injection_false():
    """The absent block defaults injection_ok to a falsy verdict."""
    block = build_radio_block(None)
    assert block["adapter_chipset"] is None
    assert block["adapter_injection_ok"] is False


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
