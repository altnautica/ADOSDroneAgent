"""Tests for WFB-ng WiFi adapter detection and monitor mode."""

from __future__ import annotations

from unittest.mock import MagicMock, patch

from ados.services.wfb.adapter import (
    _parse_iw_dev,
    _parse_phy_info,
    detect_wfb_adapters,
    set_managed_mode,
    set_monitor_mode,
)

# --- iw dev parsing ---

IW_DEV_OUTPUT = """\
phy#0
\tInterface wlan0
\t\tifindex 3
\t\twdev 0x1
\t\taddr aa:bb:cc:dd:ee:ff
\t\ttype managed
phy#1
\tInterface wlan1
\t\tifindex 4
\t\twdev 0x100000001
\t\taddr 11:22:33:44:55:66
\t\ttype monitor
"""


def test_parse_iw_dev_basic():
    result = _parse_iw_dev(IW_DEV_OUTPUT)
    assert len(result) == 2
    assert result[0]["interface"] == "wlan0"
    assert result[0]["type"] == "managed"
    assert result[1]["interface"] == "wlan1"
    assert result[1]["type"] == "monitor"


def test_parse_iw_dev_empty():
    result = _parse_iw_dev("")
    assert result == []


def test_parse_iw_dev_single_interface():
    output = "phy#0\n\tInterface wlan0\n\t\ttype managed\n"
    result = _parse_iw_dev(output)
    assert len(result) == 1
    assert result[0]["interface"] == "wlan0"


# --- iw phy parsing ---

IW_PHY_OUTPUT = """\
Wiphy phy0
\tBand 1:
\t\tCapabilities: 0x1234
\tSupported interface modes:
\t\t * managed
\t\t * AP
\t\t * monitor
\tBand 2:
Wiphy phy1
\tSupported interface modes:
\t\t * managed
\t\t * AP
"""


def test_parse_phy_info_basic():
    result = _parse_phy_info(IW_PHY_OUTPUT)
    assert "phy0" in result
    assert "monitor" in result["phy0"]
    assert "managed" in result["phy0"]
    assert "phy1" in result
    assert "monitor" not in result["phy1"]


def test_parse_phy_info_empty():
    result = _parse_phy_info("")
    assert result == {}


# --- detect_wfb_adapters ---

@patch("ados.services.wfb.adapter.platform")
def test_detect_non_linux(mock_platform):
    mock_platform.system.return_value = "Darwin"
    result = detect_wfb_adapters()
    assert result == []


@patch("ados.services.wfb.adapter.subprocess")
@patch("ados.services.wfb.adapter.discover_usb_devices")
@patch("ados.services.wfb.adapter.platform")
def test_detect_linux_with_adapters(mock_platform, mock_usb, mock_subprocess):
    mock_platform.system.return_value = "Linux"
    mock_usb.return_value = []

    # Mock iw dev
    dev_result = MagicMock()
    dev_result.returncode = 0
    dev_result.stdout = "phy#0\n\tInterface wlan0\n\t\ttype managed\n"

    # Mock iw phy
    phy_result = MagicMock()
    phy_result.returncode = 0
    phy_result.stdout = "Wiphy phy0\n\tSupported interface modes:\n\t\t * managed\n\t\t * monitor\n"

    # Mock readlink for driver
    driver_result = MagicMock()
    driver_result.returncode = 0
    driver_result.stdout = "/sys/bus/usb/drivers/rtl88xxau\n"

    mock_subprocess.run.side_effect = [dev_result, phy_result, driver_result]

    result = detect_wfb_adapters()
    assert len(result) == 1
    assert result[0].interface_name == "wlan0"
    assert result[0].supports_monitor is True
    assert result[0].driver == "rtl88xxau"


@patch("ados.services.wfb.adapter.subprocess")
@patch("ados.services.wfb.adapter.discover_usb_devices")
@patch("ados.services.wfb.adapter.platform")
def test_detect_iw_dev_fails(mock_platform, mock_usb, mock_subprocess):
    mock_platform.system.return_value = "Linux"
    mock_usb.return_value = []

    fail_result = MagicMock()
    fail_result.returncode = 1
    mock_subprocess.run.return_value = fail_result

    result = detect_wfb_adapters()
    assert result == []


# --- set_monitor_mode ---

@patch("ados.services.wfb.adapter.platform")
def test_monitor_mode_non_linux(mock_platform):
    mock_platform.system.return_value = "Darwin"
    assert set_monitor_mode("wlan0") is False


@patch("ados.services.wfb.adapter.subprocess")
@patch("ados.services.wfb.adapter.platform")
def test_monitor_mode_success(mock_platform, mock_subprocess):
    mock_platform.system.return_value = "Linux"
    ok_result = MagicMock()
    ok_result.returncode = 0
    mock_subprocess.run.return_value = ok_result

    assert set_monitor_mode("wlan0") is True
    assert mock_subprocess.run.call_count == 3


@patch("ados.services.wfb.adapter.subprocess")
@patch("ados.services.wfb.adapter.platform")
def test_monitor_mode_failure(mock_platform, mock_subprocess):
    mock_platform.system.return_value = "Linux"
    fail_result = MagicMock()
    fail_result.returncode = 1
    fail_result.stderr = "Operation not permitted"
    mock_subprocess.run.return_value = fail_result

    assert set_monitor_mode("wlan0") is False


# --- set_managed_mode ---

@patch("ados.services.wfb.adapter.platform")
def test_managed_mode_non_linux(mock_platform):
    mock_platform.system.return_value = "Darwin"
    assert set_managed_mode("wlan0") is False


@patch("ados.services.wfb.adapter.subprocess")
@patch("ados.services.wfb.adapter.platform")
def test_managed_mode_success(mock_platform, mock_subprocess):
    mock_platform.system.return_value = "Linux"
    ok_result = MagicMock()
    ok_result.returncode = 0
    mock_subprocess.run.return_value = ok_result

    assert set_managed_mode("wlan0") is True
    assert mock_subprocess.run.call_count == 3


def test_multi_radio_does_not_tag_onboard_as_rtl(monkeypatch):
    """Regression: a board with an onboard non-RTL adapter (wlan0,
    e.g. AIC8800) plus an RTL USB dongle (wlxMAC, driver=8812eu) must
    tag ONLY the RTL iface as wfb-compatible. Earlier versions
    cross-correlated by name substring then by global lsusb sweep,
    which marked the onboard adapter as compatible whenever any RTL
    happened to be plugged in elsewhere."""
    from ados.services.wfb import adapter as adapter_mod
    from ados.hal.usb import UsbCategory

    # Mock platform Linux so detection runs.
    monkeypatch.setattr(adapter_mod.platform, "system", lambda: "Linux")

    # Two netdevs visible to iw dev: wlan0 + wlxMAC.
    iw_dev_stdout = (
        "phy#0\n\tInterface wlan0\n\t\ttype managed\n"
        "phy#1\n\tInterface wlxfc23cd1cf1a5\n\t\ttype managed\n"
    )
    # Both phys advertise monitor mode so the manager-side filter
    # doesn't drop them on that axis.
    iw_phy_stdout = (
        "Wiphy phy0\n\tSupported interface modes:\n\t\t * managed\n\t\t * monitor\n"
        "Wiphy phy1\n\tSupported interface modes:\n\t\t * managed\n\t\t * monitor\n"
    )

    def _fake_subprocess_run(cmd, **_kw):
        result = MagicMock()
        result.returncode = 0
        result.stdout = ""
        if cmd[:2] == ["iw", "dev"]:
            result.stdout = iw_dev_stdout
        elif cmd[:2] == ["iw", "phy"]:
            result.stdout = iw_phy_stdout
        elif cmd[0] == "readlink":
            target = cmd[1]
            if "wlan0" in target:
                # Onboard driver (AIC8800-style) — NOT a known WFB driver.
                result.stdout = "/sys/bus/usb/drivers/aic8800_fdrv\n"
            else:
                result.stdout = "/sys/bus/usb/drivers/8812eu\n"
        return result

    monkeypatch.setattr(adapter_mod.subprocess, "run", _fake_subprocess_run)

    # USB inventory: an RTL dongle is plugged in. The previous bug
    # tagged EVERY netdev as RTL-compatible because of this.
    rtl_usb = MagicMock()
    rtl_usb.vid = 0x0BDA
    rtl_usb.pid = 0xA81A
    rtl_usb.category = UsbCategory.RADIO
    rtl_usb.name = "wlxfc23cd1cf1a5"
    rtl_usb.description = "RTL8812AU"

    monkeypatch.setattr(
        adapter_mod, "discover_usb_devices", lambda: [rtl_usb]
    )

    # Per-iface USB ID lookup: only wlxMAC's sysfs walk hits the RTL
    # vendor IDs. wlan0 returns (0, 0) because its sysfs walk reaches
    # a non-USB device or has no idVendor.
    def _fake_usb_id_for_interface(iface):
        if iface == "wlxfc23cd1cf1a5":
            return (0x0BDA, 0xA81A)
        return (0, 0)

    monkeypatch.setattr(
        adapter_mod, "_get_usb_id_for_interface", _fake_usb_id_for_interface
    )

    adapters = detect_wfb_adapters()
    by_name = {a.interface_name: a for a in adapters}

    assert "wlan0" in by_name and "wlxfc23cd1cf1a5" in by_name
    assert by_name["wlan0"].is_wfb_compatible is False, (
        "onboard non-RTL adapter must NOT be tagged as wfb compatible "
        "even when an RTL dongle is plugged in elsewhere"
    )
    assert by_name["wlxfc23cd1cf1a5"].is_wfb_compatible is True, (
        "the RTL adapter (driver=8812eu, VID:PID=0BDA:A81A) must be "
        "tagged as wfb compatible"
    )
    # Chipset label on the RTL iface should reflect the USB chipset
    # name for diagnostics, not just the bare driver string.
    assert "RTL" in by_name["wlxfc23cd1cf1a5"].chipset


def test_compat_via_driver_name_when_sysfs_walk_misses(monkeypatch):
    """Driver-name match is the authoritative signal even when the
    sysfs USB ID walk returns (0, 0). Catches RTL adapters on hub
    layouts where the parent USB device dir is one level further up."""
    from ados.services.wfb import adapter as adapter_mod

    monkeypatch.setattr(adapter_mod.platform, "system", lambda: "Linux")

    iw_dev_stdout = "phy#0\n\tInterface wlan_rtl\n\t\ttype managed\n"
    iw_phy_stdout = (
        "Wiphy phy0\n\tSupported interface modes:\n\t\t * managed\n\t\t * monitor\n"
    )

    def _fake_subprocess_run(cmd, **_kw):
        result = MagicMock()
        result.returncode = 0
        result.stdout = ""
        if cmd[:2] == ["iw", "dev"]:
            result.stdout = iw_dev_stdout
        elif cmd[:2] == ["iw", "phy"]:
            result.stdout = iw_phy_stdout
        elif cmd[0] == "readlink":
            result.stdout = "/sys/bus/usb/drivers/rtl88x2eu\n"
        return result

    monkeypatch.setattr(adapter_mod.subprocess, "run", _fake_subprocess_run)
    monkeypatch.setattr(adapter_mod, "discover_usb_devices", lambda: [])
    monkeypatch.setattr(
        adapter_mod, "_get_usb_id_for_interface", lambda _iface: (0, 0)
    )

    adapters = detect_wfb_adapters()
    assert len(adapters) == 1
    assert adapters[0].is_wfb_compatible is True
