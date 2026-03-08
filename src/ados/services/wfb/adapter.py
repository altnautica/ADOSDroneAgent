"""WiFi adapter detection and monitor mode management for WFB-ng."""

from __future__ import annotations

import platform
import subprocess
from dataclasses import dataclass, field

from ados.core.logging import get_logger
from ados.hal.usb import UsbCategory, discover_usb_devices

log = get_logger("wfb.adapter")

# Known WFB-ng compatible chipsets by VID:PID
WFB_COMPATIBLE: dict[tuple[int, int], str] = {
    (0x0BDA, 0x8812): "RTL8812AU",
    (0x0BDA, 0xB812): "RTL8812BU",
    (0x0BDA, 0x881A): "RTL8812AU (alt)",
    (0x2357, 0x0120): "RTL8812AU (TP-Link)",
    (0x2357, 0x0101): "RTL8812AU (TP-Link alt)",
}


@dataclass
class WifiAdapterInfo:
    """Information about a detected WiFi adapter."""

    interface_name: str
    driver: str
    chipset: str
    supports_monitor: bool
    current_mode: str
    phy: str = ""
    usb_vid: int = 0
    usb_pid: int = 0
    is_wfb_compatible: bool = False
    capabilities: list[str] = field(default_factory=list)


def _parse_iw_dev(output: str) -> list[dict[str, str]]:
    """Parse `iw dev` output into a list of interface dictionaries."""
    interfaces: list[dict[str, str]] = []
    current: dict[str, str] = {}

    for line in output.splitlines():
        stripped = line.strip()
        if stripped.startswith("phy#"):
            if current.get("interface"):
                interfaces.append(current)
            current = {"phy": stripped}
        elif stripped.startswith("Interface "):
            if current.get("interface"):
                interfaces.append(dict(current))
            current["interface"] = stripped.split("Interface ", 1)[1].strip()
        elif stripped.startswith("type "):
            current["type"] = stripped.split("type ", 1)[1].strip()
        elif stripped.startswith("addr "):
            current["addr"] = stripped.split("addr ", 1)[1].strip()

    if current.get("interface"):
        interfaces.append(current)

    return interfaces


def _parse_phy_info(output: str) -> dict[str, list[str]]:
    """Parse `iw phy` output to extract supported modes per phy.

    Returns mapping of phy name to list of supported interface modes.
    """
    result: dict[str, list[str]] = {}
    current_phy = ""
    in_modes_section = False

    for line in output.splitlines():
        stripped = line.strip()
        if stripped.startswith("Wiphy "):
            current_phy = stripped.split("Wiphy ", 1)[1].strip()
            result[current_phy] = []
            in_modes_section = False
        elif "Supported interface modes:" in stripped:
            in_modes_section = True
        elif in_modes_section:
            if stripped.startswith("* "):
                mode = stripped[2:].strip()
                if current_phy in result:
                    result[current_phy].append(mode)
            elif stripped and not stripped.startswith("*"):
                in_modes_section = False

    return result


def _get_driver_for_interface(interface: str) -> str:
    """Get the kernel driver name for a network interface on Linux."""
    try:
        result = subprocess.run(
            ["readlink", f"/sys/class/net/{interface}/device/driver"],
            capture_output=True,
            text=True,
            timeout=5,
        )
        if result.returncode == 0 and result.stdout.strip():
            return result.stdout.strip().rsplit("/", 1)[-1]
    except (FileNotFoundError, subprocess.TimeoutExpired):
        pass
    return "unknown"


def detect_wfb_adapters() -> list[WifiAdapterInfo]:
    """Detect WiFi adapters suitable for WFB-ng.

    On Linux, parses `iw dev` and `iw phy` to find interfaces that support
    monitor mode. Cross-references with USB device list to identify known
    WFB-ng compatible chipsets (RTL8812AU/BU).

    On macOS or other platforms, returns an empty list with a warning.
    """
    system = platform.system()
    if system != "Linux":
        log.warning("wfb_unsupported_platform", platform=system)
        return []

    # Get USB devices for cross-referencing
    usb_radios = [d for d in discover_usb_devices() if d.category == UsbCategory.RADIO]
    usb_by_name: dict[str, tuple[int, int, str]] = {}
    for dev in usb_radios:
        pair = (dev.vid, dev.pid)
        chipset = WFB_COMPATIBLE.get(pair, dev.description)
        usb_by_name[dev.name] = (dev.vid, dev.pid, chipset)

    # Parse iw dev for interfaces
    try:
        dev_result = subprocess.run(
            ["iw", "dev"],
            capture_output=True,
            text=True,
            timeout=10,
        )
        if dev_result.returncode != 0:
            log.warning("iw_dev_failed", returncode=dev_result.returncode)
            return []
    except FileNotFoundError:
        log.warning("iw_not_found")
        return []
    except subprocess.TimeoutExpired:
        log.warning("iw_dev_timeout")
        return []

    iw_interfaces = _parse_iw_dev(dev_result.stdout)

    # Parse iw phy for supported modes
    try:
        phy_result = subprocess.run(
            ["iw", "phy"],
            capture_output=True,
            text=True,
            timeout=10,
        )
        phy_modes: dict[str, list[str]] = {}
        if phy_result.returncode == 0:
            phy_modes = _parse_phy_info(phy_result.stdout)
    except (FileNotFoundError, subprocess.TimeoutExpired):
        phy_modes = {}

    adapters: list[WifiAdapterInfo] = []
    for iface in iw_interfaces:
        name = iface.get("interface", "")
        if not name:
            continue

        phy_name = iface.get("phy", "").replace("phy#", "phy")
        modes = phy_modes.get(phy_name, [])
        supports_monitor = "monitor" in modes
        current_mode = iface.get("type", "unknown")
        driver = _get_driver_for_interface(name)

        # Cross-reference with USB
        vid = 0
        pid = 0
        chipset = driver
        is_compat = False

        for usb_name, (uv, up, uc) in usb_by_name.items():
            if usb_name.lower() in name.lower() or name.lower() in usb_name.lower():
                vid, pid, chipset = uv, up, uc
                is_compat = (uv, up) in WFB_COMPATIBLE
                break

        # Also check by VID:PID directly from any radio device
        if not is_compat:
            for dev in usb_radios:
                if (dev.vid, dev.pid) in WFB_COMPATIBLE:
                    vid, pid = dev.vid, dev.pid
                    chipset = WFB_COMPATIBLE[(vid, pid)]
                    is_compat = True
                    break

        adapter = WifiAdapterInfo(
            interface_name=name,
            driver=driver,
            chipset=chipset,
            supports_monitor=supports_monitor,
            current_mode=current_mode,
            phy=phy_name,
            usb_vid=vid,
            usb_pid=pid,
            is_wfb_compatible=is_compat,
            capabilities=modes,
        )
        adapters.append(adapter)

    log.info(
        "wfb_adapter_scan",
        total=len(adapters),
        compatible=sum(1 for a in adapters if a.is_wfb_compatible),
        monitor_capable=sum(1 for a in adapters if a.supports_monitor),
    )
    return adapters


def set_monitor_mode(interface: str) -> bool:
    """Put a WiFi interface into monitor mode.

    Runs: ip link set <iface> down, iw <iface> set monitor none, ip link set <iface> up.
    Returns True on success.
    """
    system = platform.system()
    if system != "Linux":
        log.warning("monitor_mode_unsupported", platform=system)
        return False

    commands = [
        ["ip", "link", "set", interface, "down"],
        ["iw", interface, "set", "monitor", "none"],
        ["ip", "link", "set", interface, "up"],
    ]

    for cmd in commands:
        try:
            result = subprocess.run(
                cmd,
                capture_output=True,
                text=True,
                timeout=10,
            )
            if result.returncode != 0:
                log.error(
                    "monitor_mode_cmd_failed",
                    cmd=" ".join(cmd),
                    stderr=result.stderr.strip(),
                )
                return False
        except FileNotFoundError:
            log.error("monitor_mode_tool_missing", cmd=cmd[0])
            return False
        except subprocess.TimeoutExpired:
            log.error("monitor_mode_timeout", cmd=" ".join(cmd))
            return False

    log.info("monitor_mode_set", interface=interface)
    return True


def set_managed_mode(interface: str) -> bool:
    """Restore a WiFi interface to managed mode.

    Runs: ip link set <iface> down, iw <iface> set type managed, ip link set <iface> up.
    Returns True on success.
    """
    system = platform.system()
    if system != "Linux":
        log.warning("managed_mode_unsupported", platform=system)
        return False

    commands = [
        ["ip", "link", "set", interface, "down"],
        ["iw", interface, "set", "type", "managed"],
        ["ip", "link", "set", interface, "up"],
    ]

    for cmd in commands:
        try:
            result = subprocess.run(
                cmd,
                capture_output=True,
                text=True,
                timeout=10,
            )
            if result.returncode != 0:
                log.error(
                    "managed_mode_cmd_failed",
                    cmd=" ".join(cmd),
                    stderr=result.stderr.strip(),
                )
                return False
        except FileNotFoundError:
            log.error("managed_mode_tool_missing", cmd=cmd[0])
            return False
        except subprocess.TimeoutExpired:
            log.error("managed_mode_timeout", cmd=" ".join(cmd))
            return False

    log.info("managed_mode_set", interface=interface)
    return True
