"""Tests for the Connectivity step's data sources.

Covers the two backend gaps that previously left the Step 2 summary
tiles stuck on the warn-state fallback strings even when the lower
Hardware-detail panel showed everything green:

* ``NetworkStatus`` now carries ``uplink_kind`` / ``wifi_ssid`` /
  ``rssi_dbm`` / ``ip_addresses`` so the Network tile renders the
  same reality the hardware-check uplink row uses.
* ``_video_slice`` no longer hard-codes ``state="unknown"``; it
  picks between ``running``, ``ready``, and ``no_camera`` based on a
  MediaMTX liveness probe plus a kernel-side V4L2 enumeration.
"""

from __future__ import annotations

from types import SimpleNamespace

from ados.api.routes import dashboard as dashboard_route
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


# ---- _video_slice runtime probe -----------------------------------------


class _FakeConfig:
    """Minimal AgentApp-shaped config for the dashboard slice."""

    def __init__(self) -> None:
        self.encoder = SimpleNamespace(
            codec="h264",
            width=1920,
            height=1080,
            fps=30,
            bitrate_kbps=4000,
        )


class _FakeApp:
    def __init__(self) -> None:
        self.config = SimpleNamespace(video=_FakeConfig())


class TestVideoSlice:
    def test_running_when_mediamtx_alive(self, monkeypatch) -> None:
        monkeypatch.setattr(
            "ados.api.routes.video._common.mediamtx_whep_alive_sync",
            lambda: True,
        )
        monkeypatch.setattr(
            dashboard_route, "_video_devices_present", lambda: True
        )
        slice_ = dashboard_route._video_slice(_FakeApp())
        assert slice_["state"] == "running"
        assert slice_["codec"] == "h264"
        assert slice_["width"] == 1920

    def test_ready_when_camera_present_but_mediamtx_down(self, monkeypatch) -> None:
        monkeypatch.setattr(
            "ados.api.routes.video._common.mediamtx_whep_alive_sync",
            lambda: False,
        )
        monkeypatch.setattr(
            dashboard_route, "_video_devices_present", lambda: True
        )
        slice_ = dashboard_route._video_slice(_FakeApp())
        assert slice_["state"] == "ready"

    def test_no_camera_when_v4l2_empty(self, monkeypatch) -> None:
        monkeypatch.setattr(
            "ados.api.routes.video._common.mediamtx_whep_alive_sync",
            lambda: False,
        )
        monkeypatch.setattr(
            dashboard_route, "_video_devices_present", lambda: False
        )
        slice_ = dashboard_route._video_slice(_FakeApp())
        assert slice_["state"] == "no_camera"

    def test_glass_to_glass_default_preserved(self, monkeypatch) -> None:
        monkeypatch.setattr(
            "ados.api.routes.video._common.mediamtx_whep_alive_sync",
            lambda: True,
        )
        monkeypatch.setattr(
            dashboard_route, "_video_devices_present", lambda: True
        )
        slice_ = dashboard_route._video_slice(_FakeApp())
        assert slice_["glass_to_glass_ms"] is None


class TestVideoDevicesPresent:
    def test_true_when_sysfs_has_entries(self, tmp_path, monkeypatch) -> None:
        v4l = tmp_path / "video4linux"
        v4l.mkdir()
        (v4l / "video0").mkdir()
        monkeypatch.setattr(
            dashboard_route,
            "Path",
            lambda *a, **k: v4l if a == ("/sys/class/video4linux",) else None,
        )
        assert dashboard_route._video_devices_present() is True

    def test_false_when_sysfs_missing(self, monkeypatch) -> None:
        def _explode(*_a, **_k):
            raise OSError("no such directory")

        # Make iterdir blow up — the helper must swallow it.
        monkeypatch.setattr(
            dashboard_route,
            "Path",
            lambda *_a, **_kw: SimpleNamespace(iterdir=_explode),
        )
        assert dashboard_route._video_devices_present() is False
