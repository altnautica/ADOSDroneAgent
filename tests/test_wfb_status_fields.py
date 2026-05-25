"""Tests for the radio observability fields added to the status surfaces.

Asserts the new snake_case keys are present on both the transmit-side
(WfbManager.get_status) and receive-side (WfbRxManager.stats) status
dicts, and that the /api/wfb adapter introspection fallback fills in
driver / chipset / supports_monitor from sysfs + iw.
"""

from __future__ import annotations

from unittest.mock import MagicMock, patch

from ados.core.config import WfbConfig


def _drone_manager() -> "object":
    from ados.services.wfb.manager import WfbManager

    return WfbManager(
        WfbConfig(interface="wlan0", channel=149, tx_power_dbm=5, fec_k=8, fec_n=12)
    )


def test_drone_status_has_new_observability_fields():
    mgr = _drone_manager()
    status = mgr.get_status()
    for key in (
        "tx_bytes_per_s",
        "valid_rx_packets_per_s",
        "channel_locked",
        "video_inbound_bytes_per_s",
    ):
        assert key in status, f"missing {key}"
    # The transmitter owns its channel.
    assert status["channel_locked"] is True
    # Drone is the video source, never a receiver.
    assert status["video_inbound_bytes_per_s"] == 0.0
    # Rates are numeric.
    assert isinstance(status["tx_bytes_per_s"], (int, float))
    assert isinstance(status["valid_rx_packets_per_s"], (int, float))


def test_ground_stats_has_new_observability_fields():
    from ados.services.ground_station.wfb_rx import WfbRxManager

    cfg = WfbConfig(interface="wlan1", channel=157, band="u-nii-3")
    with patch(
        "ados.services.wfb.manager._apply_link_preset"
    ):
        mgr = WfbRxManager(cfg)
    stats = mgr.stats()
    for key in (
        "valid_rx_packets_per_s",
        "video_inbound_bytes_per_s",
        "tx_bytes_per_s",
        "channel_locked",
        "acquire_state",
        "locked_channel",
        "reacquire_kills",
        "rx_silent_seconds",
    ):
        assert key in stats, f"missing {key}"
    # No acquirer constructed yet (run() not entered) → idle/unlocked.
    assert stats["channel_locked"] is False
    assert stats["acquire_state"] == "idle"
    assert stats["locked_channel"] is None


def test_ground_stats_reflects_acquirer_lock():
    from ados.services.ground_station.wfb_rx import WfbRxManager
    from ados.services.wfb.channel_acquire import AcquireState, ChannelAcquirer

    cfg = WfbConfig(interface="wlan1", channel=157, band="u-nii-1")
    with patch(
        "ados.services.wfb.manager._apply_link_preset"
    ):
        mgr = WfbRxManager(cfg)
    acq = ChannelAcquirer(
        interface="wlan1",
        band="u-nii-1",
        valid_packets_fn=lambda: 0,
    )
    acq._state = AcquireState.LOCKED
    acq._locked_channel = 44
    mgr._acquirer = acq
    stats = mgr.stats()
    assert stats["channel_locked"] is True
    assert stats["acquire_state"] == "locked"
    assert stats["locked_channel"] == 44


def test_channel_locks_on_valid_decode_without_sweep():
    """THE BUG: valid video on the current channel must report locked
    even though no sweep ever ran (rig booted on the persisted channel).
    """
    from ados.services.ground_station.wfb_rx import WfbRxManager
    from ados.services.wfb.channel_acquire import AcquireState, ChannelAcquirer
    from ados.services.wfb.link_quality import LinkStats

    cfg = WfbConfig(interface="wlan1", channel=149, band="u-nii-1")
    with patch("ados.services.wfb.manager._apply_link_preset"):
        mgr = WfbRxManager(cfg)
    # Acquirer constructed (as run() would) but never swept → IDLE.
    mgr._acquirer = ChannelAcquirer(
        interface="wlan1",
        band="u-nii-1",
        valid_packets_fn=lambda: 0,
    )
    assert mgr._acquirer.state == AcquireState.IDLE
    assert mgr.stats()["channel_locked"] is False

    # A valid-decode interval lands (packets + bitrate non-zero).
    snap = LinkStats(
        rssi_dbm=-33.0, snr_db=36.0, packets_received=1000, bitrate_kbps=4096
    )
    mgr._update_rx_rates(snap)

    stats = mgr.stats()
    assert stats["channel_locked"] is True
    assert stats["acquire_state"] == "locked"
    assert stats["locked_channel"] == 149
    assert stats["valid_rx_packets_per_s"] > 0
    assert stats["video_inbound_bytes_per_s"] > 0


def test_effective_lock_state_fallback_when_acquirer_idle():
    """Even if mark_locked somehow lagged, live decodes resolve locked."""
    from ados.services.ground_station.wfb_rx import WfbRxManager
    from ados.services.wfb.channel_acquire import AcquireState, ChannelAcquirer

    cfg = WfbConfig(interface="wlan1", channel=153, band="u-nii-1")
    with patch("ados.services.wfb.manager._apply_link_preset"):
        mgr = WfbRxManager(cfg)
    mgr._acquirer = ChannelAcquirer(
        interface="wlan1",
        band="u-nii-1",
        valid_packets_fn=lambda: 0,
    )
    # Acquirer left IDLE on purpose; drive the rate field directly to
    # simulate "decoding now" without mark_locked having run.
    mgr._acquirer._state = AcquireState.IDLE
    mgr._valid_rx_packets_per_s = 120.0
    locked, state, channel = mgr._effective_lock_state()
    assert locked is True
    assert state == "locked"
    assert channel == 153


def test_api_introspect_adapter_fills_driver_and_monitor():
    from ados.api.routes import wfb as wfb_routes

    fake_proc = MagicMock()
    fake_proc.returncode = 0
    fake_proc.stdout = "Supported interface modes:\n\t\t * monitor\n"

    with patch(
        "ados.api.routes.wfb.os.readlink",
        return_value="/sys/bus/usb/drivers/rtl88x2eu",
    ), patch(
        "ados.api.routes.wfb.Path"
    ) as path_mock, patch(
        "ados.api.routes.wfb.subprocess.run", return_value=fake_proc
    ):
        path_mock.return_value.read_text.return_value = "1\n"
        info = wfb_routes._introspect_adapter("wlan0")

    assert info["driver"] == "rtl88x2eu"
    assert info["chipset"] == "RTL8812EU"
    assert info["supports_monitor"] is True


def test_api_introspect_adapter_empty_for_blank_iface():
    from ados.api.routes import wfb as wfb_routes

    info = wfb_routes._introspect_adapter("")
    assert info == {
        "driver": "",
        "chipset": "",
        "supports_monitor": False,
    }


def test_chipset_from_driver_mapping():
    from ados.api.routes.wfb import _chipset_from_driver

    assert _chipset_from_driver("rtl88x2eu") == "RTL8812EU"
    assert _chipset_from_driver("8812au") == "RTL8812AU"
    assert _chipset_from_driver("88XXau") == "RTL8812AU"
    assert _chipset_from_driver("somethingelse") == "somethingelse"
    assert _chipset_from_driver("") == ""
