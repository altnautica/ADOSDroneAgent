"""Tests for the Connectivity step's data sources.

``NetworkStatus`` carries ``uplink_kind`` / ``wifi_ssid`` / ``rssi_dbm`` /
``ip_addresses`` so the Step 2 Network tile renders the same reality the
hardware-check uplink row uses, rather than sticking on the warn-state
fallback strings while the lower Hardware-detail panel shows everything green.
"""

from __future__ import annotations

from types import SimpleNamespace

from ados.setup.models import NetworkStatus
from ados.setup.service import _net_helpers

# ---- NetworkStatus + helpers --------------------------------------------


class TestNetworkStatusShape:
    def test_defaults_remain_backwards_compatible(self) -> None:
        net = NetworkStatus()
        # Pre-existing fields stay at the old defaults so any consumer
        # still doing `network.local_ips or []` keeps working.
        assert net.local_ips == []
        assert net.hotspot_enabled is False
        # New fields are optional and default to None / empty dict so an
        # agent that hasn't run the probes yet does not lie.
        assert net.uplink_kind is None
        assert net.wifi_ssid is None
        assert net.rssi_dbm is None
        assert net.ip_addresses == {}

    def test_accepts_populated_uplink_fields(self) -> None:
        net = NetworkStatus(
            uplink_kind="ethernet",
            ip_addresses={"end0": "192.168.1.42"},
        )
        assert net.uplink_kind == "ethernet"
        assert net.ip_addresses == {"end0": "192.168.1.42"}


class TestProbeActiveUplinkKind:
    def test_returns_none_when_nothing_up(self, monkeypatch) -> None:
        monkeypatch.setattr(
            "ados.bootstrap.profile_detect.probe_uplink_kinds",
            lambda: [],
        )
        monkeypatch.setattr(
            "ados.hal.modem.detect_modem", lambda: None, raising=False
        )
        assert _net_helpers._probe_active_uplink_kind() is None

    def test_prefers_ethernet_over_wifi(self, monkeypatch) -> None:
        monkeypatch.setattr(
            "ados.bootstrap.profile_detect.probe_uplink_kinds",
            lambda: ["ethernet", "WiFi"],
        )
        monkeypatch.setattr(
            "ados.hal.modem.detect_modem", lambda: None, raising=False
        )
        assert _net_helpers._probe_active_uplink_kind() == "ethernet"

    def test_normalises_wifi_label(self, monkeypatch) -> None:
        monkeypatch.setattr(
            "ados.bootstrap.profile_detect.probe_uplink_kinds",
            lambda: ["WiFi"],
        )
        monkeypatch.setattr(
            "ados.hal.modem.detect_modem", lambda: None, raising=False
        )
        assert _net_helpers._probe_active_uplink_kind() == "wifi"

    def test_falls_back_to_cellular_when_only_modem_up(self, monkeypatch) -> None:
        monkeypatch.setattr(
            "ados.bootstrap.profile_detect.probe_uplink_kinds",
            lambda: [],
        )
        monkeypatch.setattr(
            "ados.hal.modem.detect_modem",
            lambda: SimpleNamespace(connection_state="connected"),
            raising=False,
        )
        assert _net_helpers._probe_active_uplink_kind() == "cellular"


class TestProbeWifiRssi:
    def test_rejects_out_of_range_values(self, tmp_path, monkeypatch) -> None:
        # Older drivers reported positive link-quality numbers in the same
        # column; reject anything outside the realistic dBm window so the
        # operator never sees a bogus +25 dBm reading.
        proc = tmp_path / "wireless"
        proc.write_text(
            "Inter-| sta-|   Quality        |   Discarded packets\n"
            " face | tus | link level noise |  nwid  crypt   frag\n"
            " wlan0: 0000   45.  25.  0.    0     0    0     0\n"
        )
        monkeypatch.setattr(_net_helpers, "Path", lambda *a, **k: proc)
        assert _net_helpers._probe_wifi_rssi_dbm() is None

    def test_reads_dbm_when_in_range(self, tmp_path, monkeypatch) -> None:
        proc = tmp_path / "wireless"
        proc.write_text(
            "Inter-| sta-|   Quality        |   Discarded packets\n"
            " face | tus | link level noise |  nwid  crypt   frag\n"
            " wlan0: 0000   70.  -54.  -90.    0     0    0     0\n"
        )
        monkeypatch.setattr(_net_helpers, "Path", lambda *a, **k: proc)
        assert _net_helpers._probe_wifi_rssi_dbm() == -54
