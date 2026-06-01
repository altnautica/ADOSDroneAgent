"""Tests for the heartbeat radio block builder."""

from __future__ import annotations

import json

import ados.core.radio_block as radio_block
from ados.core.radio_block import (
    _channel_to_freq,
    build_radio_block,
    compute_radio_stack_state,
    read_wfb_failover_state,
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


def test_radio_block_carries_live_sidecar_churn_fields():
    """tx_zombie_kills / tx_bytes_per_s / restart_count ride the block."""
    status = {
        "state": "connected",
        "interface": "wlan1",
        "channel": 149,
        "tx_zombie_kills": 2,
        "tx_bytes_per_s": 412345.6,
        "restart_count": 5,
    }
    block = build_radio_block(status)
    assert block["tx_zombie_kills"] == 2
    assert block["tx_bytes_per_s"] == 412345.6
    assert block["restart_count"] == 5


def test_radio_block_churn_fields_null_on_older_sidecar():
    """An older sidecar that omits the churn fields reads as null, not 0."""
    status = {
        "state": "connected",
        "interface": "wlan1",
        "channel": 149,
    }
    block = build_radio_block(status)
    assert block["tx_zombie_kills"] is None
    assert block["tx_bytes_per_s"] is None
    assert block["restart_count"] is None


def test_radio_block_absent_omits_churn_fields():
    """The absent block keeps its existing key set (churn fields are
    sidecar-only and only appear when a live status dict is present)."""
    block = build_radio_block(None)
    assert "tx_zombie_kills" not in block
    assert "tx_bytes_per_s" not in block
    assert "restart_count" not in block


# --- compute_radio_stack_state ------------------------------------------


def _point_stack_paths(monkeypatch, *, bins=(), bind=()):
    """Redirect the radio-stack artifact lookups at a controlled set.

    `bins` is the set of binary names that resolve; `bind` is the set of
    bind-artifact paths that exist. Anything not listed reads as absent.
    """
    bin_set = set(bins)
    bind_set = set(bind)

    real_is_file = radio_block.Path.is_file

    def fake_is_file(self):
        s = str(self)
        if s in bind_set:
            return True
        # Binary lookup: <dir>/<name> for the known bin dirs.
        for d in radio_block._RADIO_BIN_DIRS:
            for name in radio_block._RADIO_BIN_NAMES:
                if s == f"{d}/{name}" and name in bin_set:
                    return True
        if s in radio_block._BIND_ARTIFACT_PATHS:
            return False
        for d in radio_block._RADIO_BIN_DIRS:
            for name in radio_block._RADIO_BIN_NAMES:
                if s == f"{d}/{name}":
                    return False
        return real_is_file(self)

    monkeypatch.setattr(radio_block.Path, "is_file", fake_is_file)


def test_radio_stack_state_stack_incomplete_when_bins_missing(monkeypatch):
    _point_stack_paths(monkeypatch, bins=(), bind=radio_block._BIND_ARTIFACT_PATHS)
    state = compute_radio_stack_state({"adapter_injection_ok": True, "paired": True})
    assert state == "stack_incomplete"


def test_radio_stack_state_no_bind_artifacts(monkeypatch):
    _point_stack_paths(
        monkeypatch,
        bins=radio_block._RADIO_BIN_NAMES,
        bind=(),
    )
    state = compute_radio_stack_state({"adapter_injection_ok": True, "paired": True})
    assert state == "no_bind_artifacts"


def test_radio_stack_state_no_injection(monkeypatch):
    _point_stack_paths(
        monkeypatch,
        bins=radio_block._RADIO_BIN_NAMES,
        bind=radio_block._BIND_ARTIFACT_PATHS,
    )
    state = compute_radio_stack_state({"adapter_injection_ok": False, "paired": False})
    assert state == "no_injection"


def test_radio_stack_state_unpaired(monkeypatch):
    _point_stack_paths(
        monkeypatch,
        bins=radio_block._RADIO_BIN_NAMES,
        bind=radio_block._BIND_ARTIFACT_PATHS,
    )
    state = compute_radio_stack_state({"adapter_injection_ok": True, "paired": False})
    assert state == "unpaired"


def test_radio_stack_state_ok(monkeypatch):
    _point_stack_paths(
        monkeypatch,
        bins=radio_block._RADIO_BIN_NAMES,
        bind=radio_block._BIND_ARTIFACT_PATHS,
    )
    state = compute_radio_stack_state({"adapter_injection_ok": True, "paired": True})
    assert state == "ok"


def test_radio_stack_state_handles_none_status(monkeypatch):
    """A None status (no sidecar) with a complete stack reads no_injection."""
    _point_stack_paths(
        monkeypatch,
        bins=radio_block._RADIO_BIN_NAMES,
        bind=radio_block._BIND_ARTIFACT_PATHS,
    )
    assert compute_radio_stack_state(None) == "no_injection"


def test_radio_stack_state_is_one_of_the_known_set(monkeypatch):
    """The verdict is always within the GCS-clamped string set."""
    allowed = {
        "ok",
        "no_injection",
        "unpaired",
        "no_bind_artifacts",
        "stack_incomplete",
    }
    _point_stack_paths(
        monkeypatch,
        bins=radio_block._RADIO_BIN_NAMES,
        bind=radio_block._BIND_ARTIFACT_PATHS,
    )
    for status in (None, {}, {"adapter_injection_ok": True, "paired": True}):
        assert compute_radio_stack_state(status) in allowed


# --- read_wfb_failover_state --------------------------------------------


def test_read_wfb_failover_state_defaults_local_when_absent(tmp_path, monkeypatch):
    missing = tmp_path / "nope.json"
    monkeypatch.setattr(radio_block, "_FAILOVER_STATE_PATH", str(missing))
    assert read_wfb_failover_state() == "local"


def test_read_wfb_failover_state_reads_cloud_relay(tmp_path, monkeypatch):
    f = tmp_path / "wfb_failover.json"
    f.write_text(json.dumps({"state": "cloud_relay"}), encoding="utf-8")
    monkeypatch.setattr(radio_block, "_FAILOVER_STATE_PATH", str(f))
    assert read_wfb_failover_state() == "cloud_relay"


def test_read_wfb_failover_state_reads_failed(tmp_path, monkeypatch):
    f = tmp_path / "wfb_failover.json"
    f.write_text(json.dumps({"state": "failed"}), encoding="utf-8")
    monkeypatch.setattr(radio_block, "_FAILOVER_STATE_PATH", str(f))
    assert read_wfb_failover_state() == "failed"


def test_read_wfb_failover_state_clamps_unknown_to_local(tmp_path, monkeypatch):
    f = tmp_path / "wfb_failover.json"
    f.write_text(json.dumps({"state": "warp_drive"}), encoding="utf-8")
    monkeypatch.setattr(radio_block, "_FAILOVER_STATE_PATH", str(f))
    assert read_wfb_failover_state() == "local"


def test_read_wfb_failover_state_handles_garbage_file(tmp_path, monkeypatch):
    f = tmp_path / "wfb_failover.json"
    f.write_text("{not json", encoding="utf-8")
    monkeypatch.setattr(radio_block, "_FAILOVER_STATE_PATH", str(f))
    assert read_wfb_failover_state() == "local"


def test_read_wfb_failover_state_handles_non_object(tmp_path, monkeypatch):
    f = tmp_path / "wfb_failover.json"
    f.write_text(json.dumps(["local"]), encoding="utf-8")
    monkeypatch.setattr(radio_block, "_FAILOVER_STATE_PATH", str(f))
    assert read_wfb_failover_state() == "local"
