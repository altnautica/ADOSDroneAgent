"""Tests for USB device discovery and categorization."""

from __future__ import annotations

from unittest.mock import patch

from ados.hal.usb import (
    UsbCategory,
    _parse_lsusb_output,
    _parse_macos_usb_output,
    categorize_device,
    discover_usb_devices,
)

# --- categorize_device ---

def test_categorize_ftdi():
    """FTDI VID should be categorized as flight controller."""
    desc, cat = categorize_device(0x0403, 0x6001, "")
    assert cat == UsbCategory.FC
    assert "FTDI" in desc


def test_categorize_stm32():
    """STM32 VID should be categorized as flight controller."""
    desc, cat = categorize_device(0x0483, 0x5740, "")
    assert cat == UsbCategory.FC
    assert "STM32" in desc


def test_categorize_cp210x():
    """Silicon Labs CP210x should be categorized as flight controller."""
    desc, cat = categorize_device(0x10C4, 0xEA60, "")
    assert cat == UsbCategory.FC


def test_categorize_rtlsdr():
    """Realtek VID should be categorized as radio."""
    desc, cat = categorize_device(0x0BDA, 0x2838, "")
    assert cat == UsbCategory.RADIO


def test_categorize_ublox():
    """u-blox VID should be categorized as GPS."""
    desc, cat = categorize_device(0x1546, 0x01A8, "")
    assert cat == UsbCategory.GPS


def test_categorize_exact_vidpid_match():
    """Exact VID:PID should take priority over VID-only match."""
    desc, cat = categorize_device(0x0BDA, 0x8812, "")
    assert cat == UsbCategory.RADIO
    assert "RTL8812AU" in desc


def test_categorize_webcam_by_vidpid():
    """Known webcam VID:PID should be categorized as camera."""
    desc, cat = categorize_device(0x046D, 0x0825, "Logitech")
    assert cat == UsbCategory.CAMERA


def test_categorize_camera_by_name():
    """Unknown VID but name contains 'camera' should be categorized as camera."""
    desc, cat = categorize_device(0x9999, 0x0001, "HD USB Camera")
    assert cat == UsbCategory.CAMERA


def test_categorize_unknown_device():
    """Unknown VID/PID with no camera-like name should be OTHER."""
    desc, cat = categorize_device(0xFFFF, 0xFFFF, "Mystery Gadget")
    assert cat == UsbCategory.OTHER
    assert "Mystery Gadget" in desc


def test_categorize_unknown_empty_name():
    """Unknown device with empty name should get a fallback description."""
    desc, cat = categorize_device(0xFFFF, 0xFFFF, "")
    assert cat == UsbCategory.OTHER
    assert desc == "Unknown USB Device"


# --- _parse_lsusb_output ---

def test_parse_lsusb_single_device():
    """Should parse a single lsusb line."""
    output = "Bus 001 Device 003: ID 0483:5740 STM32 Virtual COM Port\n"
    devices = _parse_lsusb_output(output)
    assert len(devices) == 1
    assert devices[0].vid == 0x0483
    assert devices[0].pid == 0x5740
    assert devices[0].bus == "001"
    assert devices[0].device == "003"
    assert devices[0].category == UsbCategory.FC


def test_parse_lsusb_multiple_devices():
    """Should parse multiple lsusb lines."""
    output = (
        "Bus 001 Device 001: ID 1d6b:0002 Linux Foundation 2.0 root hub\n"
        "Bus 001 Device 003: ID 0483:5740 STM32 Virtual COM Port\n"
        "Bus 002 Device 005: ID 1546:01a8 u-blox AG\n"
    )
    devices = _parse_lsusb_output(output)
    assert len(devices) == 3
    assert devices[1].category == UsbCategory.FC
    assert devices[2].category == UsbCategory.GPS


def test_parse_lsusb_empty_output():
    """Empty output should return empty list."""
    devices = _parse_lsusb_output("")
    assert devices == []


def test_parse_lsusb_malformed_line():
    """Malformed lines should be skipped."""
    output = "This is not a valid lsusb line\nBus 001 Device 003: ID 0403:6001 FTDI\n"
    devices = _parse_lsusb_output(output)
    assert len(devices) == 1


# --- _parse_macos_usb_output ---

def test_parse_macos_usb_output():
    """Should parse system_profiler SPUSBDataType output."""
    output = """USB:

    USB 3.1 Bus:

      Host Controller Driver: AppleT8112USBXHCI

        STM32 Virtual COM Port:

          Product ID: 0x5740
          Vendor ID: 0x0483
          Location ID: 0x01100000

        u-blox 8:

          Product ID: 0x01a8
          Vendor ID: 0x1546
          Location ID: 0x01200000
"""
    devices = _parse_macos_usb_output(output)
    assert len(devices) == 2
    assert devices[0].vid == 0x0483
    assert devices[0].pid == 0x5740
    assert devices[0].category == UsbCategory.FC
    assert devices[1].vid == 0x1546
    assert devices[1].category == UsbCategory.GPS


def test_parse_macos_empty():
    """Empty macOS output should return empty list."""
    devices = _parse_macos_usb_output("")
    assert devices == []


# --- discover_usb_devices with mock subprocess ---

@patch("ados.hal.usb.platform")
@patch("ados.hal.usb.subprocess.run")
def test_discover_linux(mock_run, mock_platform):
    """Should call lsusb on Linux."""
    mock_platform.system.return_value = "Linux"
    mock_run.return_value = type("Result", (), {
        "returncode": 0,
        "stdout": "Bus 001 Device 003: ID 0483:5740 STM32 VCP\n",
        "stderr": "",
    })()

    devices = discover_usb_devices()
    mock_run.assert_called_once()
    assert len(devices) == 1
    assert devices[0].category == UsbCategory.FC


@patch("ados.hal.usb.platform")
@patch("ados.hal.usb.subprocess.run")
def test_discover_macos(mock_run, mock_platform):
    """Should call system_profiler on macOS."""
    mock_platform.system.return_value = "Darwin"
    mock_run.return_value = type("Result", (), {
        "returncode": 0,
        "stdout": """USB:
        FTDI Device:
          Product ID: 0x6001
          Vendor ID: 0x0403
          Location ID: 0x01100000
""",
        "stderr": "",
    })()

    devices = discover_usb_devices()
    mock_run.assert_called_once()
    assert len(devices) == 1
    assert devices[0].category == UsbCategory.FC


@patch("ados.hal.usb.platform")
def test_discover_unsupported_platform(mock_platform):
    """Should return empty list on unsupported platform."""
    mock_platform.system.return_value = "Windows"
    devices = discover_usb_devices()
    assert devices == []


@patch("ados.hal.usb.platform")
@patch("ados.hal.usb.subprocess.run")
def test_discover_tool_not_found(mock_run, mock_platform):
    """Should handle missing lsusb gracefully."""
    mock_platform.system.return_value = "Linux"
    mock_run.side_effect = FileNotFoundError("lsusb not found")

    devices = discover_usb_devices()
    assert devices == []


@patch("ados.hal.usb.platform")
@patch("ados.hal.usb.subprocess.run")
def test_discover_lsusb_nonzero_exit(mock_run, mock_platform):
    """Should return empty list when lsusb exits with error."""
    mock_platform.system.return_value = "Linux"
    mock_run.return_value = type("Result", (), {
        "returncode": 1,
        "stdout": "",
        "stderr": "unable to initialize libusb",
    })()

    devices = discover_usb_devices()
    assert devices == []
