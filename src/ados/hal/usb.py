"""USB device discovery and categorization for drone peripherals."""

from __future__ import annotations

import platform
import re
import subprocess
from dataclasses import dataclass
from enum import StrEnum

from ados.core.logging import get_logger

log = get_logger("hal.usb")


class UsbCategory(StrEnum):
    FC = "fc"
    RADIO = "radio"
    CAMERA = "camera"
    GPS = "gps"
    LORA = "lora"
    OTHER = "other"


# VID -> (description, category) lookup table for common drone peripherals.
# Multiple PIDs may share a VID, so we match on VID first, then refine.
VID_TABLE: dict[int, tuple[str, UsbCategory]] = {
    # Flight controllers
    0x0403: ("FTDI USB-Serial", UsbCategory.FC),
    0x0483: ("STM32 / Semtech", UsbCategory.FC),  # Also LoRa; refined by PID
    0x10C4: ("Silicon Labs CP210x", UsbCategory.FC),
    0x067B: ("Prolific PL2303", UsbCategory.FC),
    # Radio
    0x0BDA: ("RTL-SDR / Realtek", UsbCategory.RADIO),
    0x2357: ("SiK Radio", UsbCategory.RADIO),
    # GPS
    0x1546: ("u-blox GPS", UsbCategory.GPS),
    0x1209: ("Open-Source Hardware (pid.codes)", UsbCategory.FC),
}

# More specific VID:PID pairs override the VID-only lookup.
VID_PID_TABLE: dict[tuple[int, int], tuple[str, UsbCategory]] = {
    # LoRa modules on STM32 VID
    (0x0483, 0x5740): ("STM32 Virtual COM Port", UsbCategory.FC),
    (0x0483, 0xDF11): ("STM32 DFU Bootloader", UsbCategory.FC),
    # Common webcams
    (0x046D, 0x0825): ("Logitech Webcam", UsbCategory.CAMERA),
    (0x046D, 0x082D): ("Logitech HD Pro Webcam C920", UsbCategory.CAMERA),
    (0x046D, 0x0843): ("Logitech Webcam C930e", UsbCategory.CAMERA),
    (0x0C45, 0x6366): ("Generic USB Camera", UsbCategory.CAMERA),
    # RTL8812 family (used for WFB-ng video link)
    (0x0BDA, 0x8812): ("RTL8812AU WiFi (Video Link)", UsbCategory.RADIO),
    (0x0BDA, 0x881A): ("RTL8812AU WiFi (Video Link)", UsbCategory.RADIO),
    (0x0BDA, 0x881B): ("RTL8812AU WiFi (Video Link)", UsbCategory.RADIO),
    (0x0BDA, 0x881C): ("RTL8812AU WiFi (Video Link)", UsbCategory.RADIO),
    (0x0BDA, 0xB812): ("RTL8812EU WiFi (Video Link)", UsbCategory.RADIO),
    # u-blox specific
    (0x1546, 0x01A8): ("u-blox 8 GPS", UsbCategory.GPS),
    (0x1546, 0x01A7): ("u-blox 7 GPS", UsbCategory.GPS),
    (0x1209, 0x5741): ("SpeedyBee F405 V4", UsbCategory.FC),
    (0x0EDE, 0x8093): ("HZ USB Camera", UsbCategory.CAMERA),
}


@dataclass
class UsbDevice:
    vid: int
    pid: int
    name: str
    bus: str
    device: str
    description: str
    category: UsbCategory


def categorize_device(vid: int, pid: int, name: str) -> tuple[str, UsbCategory]:
    """Look up VID:PID in known tables to determine description and category."""
    # Try exact VID:PID match first
    pair = (vid, pid)
    if pair in VID_PID_TABLE:
        return VID_PID_TABLE[pair]

    # Fall back to VID-only match
    if vid in VID_TABLE:
        return VID_TABLE[vid]

    # Use the reported name for camera heuristic
    name_lower = name.lower()
    if any(kw in name_lower for kw in ("camera", "webcam", "video", "uvc")):
        return (name or "USB Camera", UsbCategory.CAMERA)

    return (name or "Unknown USB Device", UsbCategory.OTHER)


def _parse_lsusb_output(output: str) -> list[UsbDevice]:
    """Parse `lsusb` output lines into UsbDevice objects.

    Expected format: Bus 001 Device 003: ID 0483:5740 STM32 Virtual COM Port
    """
    devices: list[UsbDevice] = []
    pattern = re.compile(
        r"Bus\s+(\d+)\s+Device\s+(\d+):\s+ID\s+([0-9a-fA-F]{4}):([0-9a-fA-F]{4})\s*(.*)"
    )

    for line in output.strip().splitlines():
        m = pattern.match(line.strip())
        if not m:
            continue

        bus = m.group(1)
        device_num = m.group(2)
        vid = int(m.group(3), 16)
        pid = int(m.group(4), 16)
        raw_name = m.group(5).strip()

        description, category = categorize_device(vid, pid, raw_name)

        devices.append(UsbDevice(
            vid=vid,
            pid=pid,
            name=raw_name,
            bus=bus,
            device=device_num,
            description=description,
            category=category,
        ))

    return devices


def _parse_macos_usb_output(output: str) -> list[UsbDevice]:
    """Parse `system_profiler SPUSBDataType` output into UsbDevice objects.

    This output is hierarchical text. We look for blocks with Vendor ID / Product ID.
    """
    devices: list[UsbDevice] = []

    # Split into blocks by device name (lines ending with ':' at moderate indent)
    current_name = ""
    current_vid = 0
    current_pid = 0
    current_bus = ""

    vid_re = re.compile(r"Vendor ID:\s*0x([0-9a-fA-F]+)")
    pid_re = re.compile(r"Product ID:\s*0x([0-9a-fA-F]+)")
    location_re = re.compile(r"Location ID:\s*0x([0-9a-fA-F]+)")
    name_re = re.compile(r"^\s{4,12}(\S.*?):\s*$")

    for line in output.splitlines():
        name_match = name_re.match(line)
        if name_match:
            # Flush previous device if we had VID+PID
            if current_vid and current_pid:
                description, category = categorize_device(current_vid, current_pid, current_name)
                devices.append(UsbDevice(
                    vid=current_vid,
                    pid=current_pid,
                    name=current_name,
                    bus=current_bus,
                    device="",
                    description=description,
                    category=category,
                ))
            current_name = name_match.group(1)
            current_vid = 0
            current_pid = 0
            current_bus = ""
            continue

        vid_match = vid_re.search(line)
        if vid_match:
            current_vid = int(vid_match.group(1), 16)
            continue

        pid_match = pid_re.search(line)
        if pid_match:
            current_pid = int(pid_match.group(1), 16)
            continue

        loc_match = location_re.search(line)
        if loc_match:
            current_bus = loc_match.group(1)

    # Flush last device
    if current_vid and current_pid:
        description, category = categorize_device(current_vid, current_pid, current_name)
        devices.append(UsbDevice(
            vid=current_vid,
            pid=current_pid,
            name=current_name,
            bus=current_bus,
            device="",
            description=description,
            category=category,
        ))

    return devices


def discover_usb_devices() -> list[UsbDevice]:
    """Scan connected USB devices using platform-appropriate tools.

    On Linux: uses `lsusb`
    On macOS: uses `system_profiler SPUSBDataType`
    On other platforms: returns empty list.
    """
    system = platform.system()

    try:
        if system == "Linux":
            result = subprocess.run(
                ["lsusb"],
                capture_output=True,
                text=True,
                timeout=10,
            )
            if result.returncode == 0:
                devices = _parse_lsusb_output(result.stdout)
                log.info("usb_scan_complete", count=len(devices), platform="linux")
                return devices
            log.warning("lsusb_failed", returncode=result.returncode, stderr=result.stderr)
            return []

        elif system == "Darwin":
            result = subprocess.run(
                ["system_profiler", "SPUSBDataType"],
                capture_output=True,
                text=True,
                timeout=15,
            )
            if result.returncode == 0:
                devices = _parse_macos_usb_output(result.stdout)
                log.info("usb_scan_complete", count=len(devices), platform="macos")
                return devices
            log.warning("system_profiler_failed", returncode=result.returncode)
            return []

        else:
            log.info("usb_scan_unsupported", platform=system)
            return []

    except FileNotFoundError as e:
        log.warning("usb_tool_not_found", error=str(e))
        return []
    except subprocess.TimeoutExpired:
        log.warning("usb_scan_timeout", platform=system)
        return []
