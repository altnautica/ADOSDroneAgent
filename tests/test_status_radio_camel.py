"""Tests for the consolidated status radio camelCase converter."""

from __future__ import annotations

from ados.api.routes.status import _radio_to_camel


def test_radio_to_camel_maps_wfb_keys():
    out = _radio_to_camel(
        {
            "state": "connected",
            "rssi_dbm": -55,
            "snr_db": 30.0,
            "noise_dbm": -90.0,
            "loss_percent": 1.0,
            "mcs_index": 1,
            "rx_silent_seconds": 0.0,
            "freq_mhz": 5745,
            "paired_with_device_id": "groundnode",
        }
    )
    assert out["state"] == "connected"
    assert out["rssiDbm"] == -55
    assert out["snrDb"] == 30.0
    assert out["noiseDbm"] == -90.0
    assert out["lossPercent"] == 1.0
    assert out["mcsIndex"] == 1
    assert out["rxSilentSeconds"] == 0.0
    assert out["freqMhz"] == 5745
    assert out["pairedWithDeviceId"] == "groundnode"


def test_radio_to_camel_none_passthrough():
    assert _radio_to_camel(None) is None
    assert _radio_to_camel({}) is None
