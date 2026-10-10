"""Tests for the cross-process WFB adapter probe read seam.

The probe resolves the adapter list through the radio service's sidecar
first, then the radio binary's one-shot scan, and is empty when neither
answers. These tests exercise the pure record-normalisation logic and the
sidecar-first resolution, stubbing the binary so nothing touches a real
radio.
"""

from __future__ import annotations

import json

import pytest

from ados.services.wfb import adapter_probe as probe
from ados.services.wfb.adapter import WifiAdapterInfo

# --- _adapter_from_record: null normalisation ---


def test_record_normalises_rust_nulls_to_dataclass_defaults():
    """Rust emits null for unknown current_mode / usb ids; the dataclass
    uses "" and 0. The probe must normalise so a consumer cannot tell
    which process produced the record."""
    record = {
        "interface_name": "wlan1",
        "driver": "8812eu",
        "chipset": "RTL8812EU",
        "supports_monitor": True,
        "current_mode": None,
        "phy": "phy1",
        "usb_vid": None,
        "usb_pid": None,
        "is_wfb_compatible": True,
        "capabilities": ["managed", "monitor"],
    }
    adapter = probe._adapter_from_record(record)
    assert isinstance(adapter, WifiAdapterInfo)
    assert adapter.interface_name == "wlan1"
    assert adapter.current_mode == ""  # null -> ""
    assert adapter.usb_vid == 0  # null -> 0
    assert adapter.usb_pid == 0
    assert adapter.driver == "8812eu"
    assert adapter.is_wfb_compatible is True
    assert adapter.capabilities == ["managed", "monitor"]


def test_record_preserves_populated_values():
    record = {
        "interface_name": "wlxfc23cd1cf1a5",
        "driver": "rtl88x2eu",
        "chipset": "RTL8812EU (a81a)",
        "supports_monitor": True,
        "current_mode": "monitor",
        "phy": "phy2",
        "usb_vid": 0x0BDA,
        "usb_pid": 0xA81A,
        "is_wfb_compatible": True,
        "capabilities": ["monitor"],
    }
    adapter = probe._adapter_from_record(record)
    assert adapter.current_mode == "monitor"
    assert adapter.usb_vid == 0x0BDA
    assert adapter.usb_pid == 0xA81A


def test_record_without_interface_name_is_dropped():
    assert probe._adapter_from_record({"driver": "8812eu"}) is None
    assert probe._adapter_from_record({"interface_name": ""}) is None


# --- _parse_adapter_list ---


def test_parse_list_returns_none_for_non_array():
    assert probe._parse_adapter_list({"interface_name": "wlan0"}) is None
    assert probe._parse_adapter_list("nope") is None


def test_parse_list_drops_malformed_entries_keeps_valid():
    payload = [
        {"interface_name": "wlan0", "driver": "8812eu"},
        "garbage",
        {"no_name": True},
        {"interface_name": "wlan1", "driver": "rtl88x2eu"},
    ]
    adapters = probe._parse_adapter_list(payload)
    names = [a.interface_name for a in adapters]
    assert names == ["wlan0", "wlan1"]


def test_parse_empty_list_is_empty():
    assert probe._parse_adapter_list([]) == []


# --- detect_wfb_adapters: sidecar-first resolution ---


def _write_sidecar(path, payload):
    path.write_text(json.dumps(payload), encoding="utf-8")


def test_detect_reads_fresh_sidecar(tmp_path, monkeypatch):
    """A fresh adapters sidecar short-circuits the binary scan."""
    sidecar = tmp_path / "wfb-adapters.json"
    _write_sidecar(
        sidecar,
        [
            {
                "interface_name": "wlan1",
                "driver": "8812eu",
                "chipset": "RTL8812EU",
                "supports_monitor": True,
                "current_mode": "monitor",
                "is_wfb_compatible": True,
            }
        ],
    )
    monkeypatch.setattr(probe, "WFB_ADAPTERS_JSON", sidecar)

    # Trip-wire: the binary scan may not run.
    monkeypatch.setattr(
        probe, "_run_radio_adapters_cli", lambda: pytest.fail("binary ran")
    )

    adapters = probe.detect_wfb_adapters()
    assert len(adapters) == 1
    assert adapters[0].interface_name == "wlan1"
    assert adapters[0].is_wfb_compatible is True


def test_detect_skips_stale_sidecar_uses_binary(tmp_path, monkeypatch):
    """A stale sidecar is ignored; the binary scan is tried next."""
    sidecar = tmp_path / "wfb-adapters.json"
    _write_sidecar(sidecar, [{"interface_name": "stale0"}])
    # Backdate so the freshness gate rejects it.
    import os

    old = sidecar.stat().st_mtime - (probe._SIDECAR_FRESH_S + 60.0)
    os.utime(sidecar, (old, old))
    monkeypatch.setattr(probe, "WFB_ADAPTERS_JSON", sidecar)

    from_binary = [
        WifiAdapterInfo(
            interface_name="binwlan",
            driver="8812eu",
            chipset="RTL8812EU",
            supports_monitor=True,
            current_mode="monitor",
            is_wfb_compatible=True,
        )
    ]
    monkeypatch.setattr(probe, "_run_radio_adapters_cli", lambda: from_binary)

    adapters = probe.detect_wfb_adapters()
    assert [a.interface_name for a in adapters] == ["binwlan"]


def test_detect_is_empty_when_nothing_answers(tmp_path, monkeypatch):
    """No sidecar + no binary -> no adapters; Python runs no scan of its own."""
    monkeypatch.setattr(probe, "WFB_ADAPTERS_JSON", tmp_path / "absent.json")
    monkeypatch.setattr(probe, "_run_radio_adapters_cli", lambda: None)

    assert probe.detect_wfb_adapters() == []


def test_the_binary_is_not_run_when_absent(tmp_path, monkeypatch):
    """A missing radio binary is a clean miss, not a subprocess error."""
    monkeypatch.setattr(probe, "RADIO_BIN", str(tmp_path / "no-such-ados-radio"))
    assert probe._run_radio_adapters_cli() is None
