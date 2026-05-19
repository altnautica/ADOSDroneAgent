"""WiFi adapter detection and monitor mode management for WFB-ng."""

from __future__ import annotations

import platform
import re
import subprocess
from dataclasses import dataclass, field
from pathlib import Path

from ados.core.logging import get_logger
from ados.hal.usb import UsbCategory, discover_usb_devices

log = get_logger("wfb.adapter")

# Known WFB-ng compatible chipsets by VID:PID.
# RTL8812AU family (0x8812, 0x881A-C), RTL8812EU / RTL8822E (0xB812 /
# 0xA81A), and TP-Link rebadges all share the vendored DKMS driver and
# support monitor mode with frame injection.
WFB_COMPATIBLE: dict[tuple[int, int], str] = {
    (0x0BDA, 0x8812): "RTL8812AU",
    (0x0BDA, 0x881A): "RTL8812AU (alt)",
    (0x0BDA, 0x881B): "RTL8812AU (alt)",
    (0x0BDA, 0x881C): "RTL8812AU (alt)",
    # Ambiguous PID: shipped on both RTL8812AU rebadges and RTL8812EU /
    # RTL8822EU dongles. Default label is the AU variant; the detection
    # path below promotes it to "RTL8812EU (a81a)" when the bound kernel
    # driver is rtl88x2eu, which is the authoritative disambiguator.
    (0x0BDA, 0xA81A): "RTL8812AU (a81a)",
    (0x0BDA, 0xB812): "RTL8812EU",
    (0x2357, 0x0120): "RTL8812AU (TP-Link)",
    (0x2357, 0x0101): "RTL8812AU (TP-Link alt)",
}

# Driver-name fallback for boards whose VID:PID is not yet in the table
# above. The DKMS module exposes itself under one of these names; if
# any matches, treat the adapter as WFB-ng compatible regardless of
# the USB ID lookup. Future Realtek rebadges automatically work.
WFB_COMPATIBLE_DRIVERS: set[str] = {
    "8812au",
    "8812eu",
    "rtl8812au",
    "rtl8812eu",
    "rtl88x2eu",
    "rtl88xxau",
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


_VALID_INTERFACE_RE = re.compile(r"^[a-zA-Z0-9_-]+$")


def _validate_interface_name(interface: str) -> None:
    """Validate that an interface name contains only safe characters.

    Raises:
        ValueError: If the interface name contains disallowed characters.
    """
    if not _VALID_INTERFACE_RE.match(interface):
        raise ValueError(
            f"Invalid interface name: {interface!r}. "
            "Only alphanumeric characters, hyphens, and underscores are allowed."
        )


def _get_driver_for_interface(interface: str) -> str:
    """Get the kernel driver name for a network interface on Linux."""
    _validate_interface_name(interface)
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


def _get_usb_id_for_interface(interface: str) -> tuple[int, int]:
    """Read the USB VID:PID for a netdev by walking its sysfs device tree.

    Returns (0, 0) when the interface is not USB-backed (built-in PCI
    wireless, virtual, etc.) or when sysfs cannot be read. Walks up the
    `/sys/class/net/<iface>/device` chain because the immediate `device`
    symlink for a USB netdev points at the per-interface USB endpoint,
    not the parent USB device that owns idVendor/idProduct.
    """
    _validate_interface_name(interface)
    try:
        device_link = Path(f"/sys/class/net/{interface}/device").resolve()
    except OSError:
        return (0, 0)
    cursor: Path | None = device_link
    # Walk up until idVendor + idProduct appear. USB netdevs need this
    # because the immediate device dir is the usb interface (e.g. 1-1:1.0)
    # whose parent is the usb device (e.g. 1-1) where the IDs live.
    for _ in range(8):  # bounded so a corrupted link can't loop forever
        if cursor is None:
            break
        vendor_path = cursor / "idVendor"
        product_path = cursor / "idProduct"
        if vendor_path.is_file() and product_path.is_file():
            try:
                vid = int(vendor_path.read_text().strip(), 16)
                pid = int(product_path.read_text().strip(), 16)
                return (vid, pid)
            except (OSError, ValueError):
                return (0, 0)
        parent = cursor.parent
        if parent == cursor:
            break
        cursor = parent
    return (0, 0)


def detect_wfb_adapters() -> list[WifiAdapterInfo]:
    """Detect WiFi adapters suitable for WFB-ng.

    On Linux, parses `iw dev` and `iw phy` to find interfaces that support
    monitor mode. Cross-references with USB device list to identify known
    WFB-ng compatible chipsets (RTL8812AU family and RTL8812EU).

    On macOS or other platforms, returns an empty list with a warning.
    """
    system = platform.system()
    if system != "Linux":
        log.warning("wfb_unsupported_platform", platform=system)
        return []

    # USB radio inventory used downstream only to enrich chipset labels
    # for diagnostics. The actual per-iface USB ID lookup walks sysfs
    # off the iface itself; do not cross-correlate by name or by global
    # presence — that produces false positives on multi-radio boards.
    usb_radios = [d for d in discover_usb_devices() if d.category == UsbCategory.RADIO]

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

        # Per-iface USB ID lookup via sysfs walk. Lifts the VID:PID off
        # THIS iface's own USB device tree so we don't cross-contaminate
        # one iface's compat verdict with a different RTL dongle that
        # happens to be plugged into the same box. The previous
        # "any RTL in lsusb -> every iface is RTL-compat" fallback
        # tagged onboard adapters (e.g. AIC8800) as RTL on multi-radio
        # boards, the manager then tried monitor mode on the wrong
        # iface and got EIO.
        vid, pid = _get_usb_id_for_interface(name)
        chipset = driver
        is_compat = False

        if (vid, pid) in WFB_COMPATIBLE:
            chipset = WFB_COMPATIBLE[(vid, pid)]
            # Disambiguate the (0BDA:A81A) PID: same USB ID ships on
            # both RTL8812AU rebadges and RTL8812EU / RTL8822EU dongles.
            # The bound kernel driver is the authoritative signal — if
            # rtl88x2eu claimed the device, this is the EU silicon.
            if (
                (vid, pid) == (0x0BDA, 0xA81A)
                and driver
                and driver.lower() == "rtl88x2eu"
            ):
                chipset = "RTL8812EU (a81a)"
            is_compat = True
        elif driver and driver.lower() in WFB_COMPATIBLE_DRIVERS:
            # Driver-name confirmation: the kernel driver bound to this
            # iface is one of the known WFB-ng modules. Authoritative
            # signal that this is an RTL netdev even when sysfs walk
            # missed the IDs (e.g., USB hub layer hides the parent).
            is_compat = True
            if not chipset or chipset == driver:
                chipset = driver
        # USB lookup hint: surface the original USB device's chipset
        # label when it shares VID:PID with a known WFB chipset, so
        # diagnostic logs read e.g. "RTL8812AU (a81a)" not "8812eu".
        if is_compat and chipset == driver:
            for dev in usb_radios:
                if (dev.vid, dev.pid) == (vid, pid) and (dev.vid, dev.pid) in WFB_COMPATIBLE:
                    chipset = WFB_COMPATIBLE[(dev.vid, dev.pid)]
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

    _validate_interface_name(interface)

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


def set_tx_power(interface: str, dbm: int) -> int | None:
    """Set TX power on a WiFi interface via `iw`. Returns effective dBm.

    `iw dev <iface> set txpower fixed <mBm>` where mBm is millibel-milliwatts
    (1 dBm = 100 mBm). The driver may reject very low values depending on
    the regulatory domain; on rejection this falls back through 1 → 5 → 7
    → 10 dBm and returns whichever step succeeded, or None if all failed.
    """
    system = platform.system()
    if system != "Linux":
        log.warning("set_tx_power_unsupported", platform=system)
        return None

    _validate_interface_name(interface)

    ramp = [dbm]
    for fallback in (5, 7, 10):
        if fallback > dbm and fallback not in ramp:
            ramp.append(fallback)

    for candidate_dbm in ramp:
        mbm = int(candidate_dbm) * 100
        cmd = ["iw", "dev", interface, "set", "txpower", "fixed", str(mbm)]
        try:
            result = subprocess.run(
                cmd,
                capture_output=True,
                text=True,
                timeout=5,
            )
            if result.returncode == 0:
                if candidate_dbm != dbm:
                    log.warning(
                        "wfb_txpower_fallback",
                        requested_dbm=dbm,
                        applied_dbm=candidate_dbm,
                    )
                else:
                    log.info(
                        "wfb_txpower_applied",
                        interface=interface,
                        dbm=candidate_dbm,
                    )
                return candidate_dbm
            log.debug(
                "wfb_txpower_rejected",
                interface=interface,
                dbm=candidate_dbm,
                stderr=result.stderr.strip(),
            )
        except FileNotFoundError:
            log.error("wfb_txpower_tool_missing", cmd=cmd[0])
            return None
        except subprocess.TimeoutExpired:
            log.error("wfb_txpower_timeout", interface=interface)
            return None

    log.error("wfb_txpower_all_steps_rejected", interface=interface, requested_dbm=dbm)
    return None


def set_managed_mode(interface: str) -> bool:
    """Restore a WiFi interface to managed mode.

    Runs: ip link set <iface> down, iw <iface> set type managed, ip link set <iface> up.
    Returns True on success.
    """
    system = platform.system()
    if system != "Linux":
        log.warning("managed_mode_unsupported", platform=system)
        return False

    _validate_interface_name(interface)

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
