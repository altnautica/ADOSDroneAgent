"""Tests for WFB-ng WiFi adapter detection and monitor mode."""

from __future__ import annotations

from unittest.mock import MagicMock, patch

from ados.services.wfb.adapter import (
    WifiAdapterInfo,
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
