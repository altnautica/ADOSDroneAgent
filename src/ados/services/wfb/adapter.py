"""WiFi adapter detection and monitor mode management for WFB-ng."""

from __future__ import annotations

import platform
import re
import subprocess
from collections.abc import Callable
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

# Management-WiFi deny-set. These are onboard / management station radios
# that CANNOT inject 802.11 frames in monitor mode, so wfb_tx/wfb_rx on
# them produces zero link even when `iw set monitor` reports success.
# They must never be tagged WFB-compatible, even as a fallback. The
# Rock 5C ships an AIC8800 (USB vendor 0xa69c, driver `aic8800*`) as its
# management WiFi; Broadcom `brcmfmac` is the same class on Pi-family
# boards. We deny by USB vendor AND by driver-name prefix because the
# USB sysfs walk on a hub layout can reach a parent that does not expose
# the AIC8800's idVendor, leaving only the driver string to go on — the
# original live failure was the manager auto-picking the AIC8800 because
# it slipped the filter and sorted first by bus order.
WFB_DENY_USB_VENDORS: frozenset[int] = frozenset(
    {
        0xA69C,  # AIcSemi AIC8800 family (Rock 5C management WiFi)
    }
)
WFB_DENY_DRIVER_PREFIXES: tuple[str, ...] = (
    "aic8800",   # AIC8800 / AIC8800DC / AIC8800D80 variants
    "brcmfmac",  # Broadcom FullMAC (Pi onboard WiFi)
)


def _is_denied_management_wifi(usb_vid: int, driver: str) -> bool:
    """True when an adapter is a known non-injection management radio.

    Belt-and-suspenders gate so a management WiFi (AIC8800, brcmfmac)
    can never be tagged WFB-compatible regardless of how the USB ID walk
    resolved. Matches on USB vendor first, then a driver-name prefix
    check that tolerates the `_fdrv` / `_usb` driver suffixes those
    chips bind under.
    """
    if usb_vid and usb_vid in WFB_DENY_USB_VENDORS:
        return True
    drv = (driver or "").strip().lower()
    return any(drv.startswith(prefix) for prefix in WFB_DENY_DRIVER_PREFIXES)


def control_interface() -> str | None:
    """Return the interface carrying the kernel default route, or None.

    This is the operator's control path (the iface their SSH / Mission Control
    session arrives over). The radio adapter selection and monitor-mode setup
    must never touch it: bringing it down or flipping it to monitor mode would
    sever the only management link with no fallback and strand the box. Best
    effort — a missing default route (isolated rig) returns None.
    """
    try:
        result = subprocess.run(
            ["ip", "-4", "route", "show", "default"],
            capture_output=True,
            text=True,
            timeout=5,
        )
        if result.returncode != 0:
            return None
        for line in (result.stdout or "").splitlines():
            parts = line.split()
            if parts[:1] == ["default"] and "dev" in parts:
                return parts[parts.index("dev") + 1]
    except Exception:
        # Best-effort safety probe: it must never raise into the radio
        # adapter path. Any failure (no `ip`, timeout, odd output) means
        # "control path unknown" → do not exclude/refuse anything.
        return None
    return None


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

    # The operator's control-path interface (default route) is never a WFB
    # candidate. iw normally only lists wireless ifaces, but a WiFi-client
    # uplink (e.g. the management WiFi) can be the control path, and putting
    # it in monitor mode would sever the session. Resolved once per scan.
    control_iface = control_interface()

    adapters: list[WifiAdapterInfo] = []
    for iface in iw_interfaces:
        name = iface.get("interface", "")
        if not name:
            continue

        if control_iface and name == control_iface:
            log.info("wfb_adapter_excluded_control_iface", interface=name)
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

        # Hard deny known management-WiFi radios before any compat path
        # runs. The AIC8800 (Rock 5C onboard, vendor 0xa69c, driver
        # aic8800*) and Broadcom brcmfmac advertise monitor mode but
        # cannot inject; selecting one strands the radio link. This gate
        # runs first so neither the USB-ID match nor the driver-name
        # fallback below can ever flip is_compat true for them.
        if _is_denied_management_wifi(vid, driver):
            adapters.append(
                WifiAdapterInfo(
                    interface_name=name,
                    driver=driver,
                    chipset=driver or "management-wifi",
                    supports_monitor=supports_monitor,
                    current_mode=current_mode,
                    phy=phy_name,
                    usb_vid=vid,
                    usb_pid=pid,
                    is_wfb_compatible=False,
                    capabilities=modes,
                )
            )
            continue

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


def _injection_rank(adapter: WifiAdapterInfo) -> int:
    """Sort key that floats the real injection radios to the front.

    Lower is better. The RTL8812EU silicon is the validated injection
    radio, so it ranks first; RTL8812AU rebadges next; any other adapter
    that merely passed the compat filter last. This makes selection
    independent of USB bus order so a management WiFi enumerated first
    can never win by accident.
    """
    label = (adapter.chipset or "").upper()
    driver = (adapter.driver or "").lower()
    is_eu = (
        "8812EU" in label
        or "88X2EU" in label
        or driver in {"8812eu", "rtl8812eu", "rtl88x2eu"}
    )
    is_au = (
        "8812AU" in label
        or "88XXAU" in label
        or driver in {"8812au", "rtl8812au", "rtl88xxau"}
    )
    is_known = (
        (adapter.usb_vid, adapter.usb_pid) in WFB_COMPATIBLE
        or driver in WFB_COMPATIBLE_DRIVERS
    )
    if is_eu:
        return 0
    if is_au:
        return 1
    if is_known:
        return 2
    return 3


def select_wfb_interface(
    adapters: list[WifiAdapterInfo],
    set_monitor_fn: Callable[[str], bool],
    configured_iface: str = "",
) -> str | None:
    """Pick the WFB interface, RTL-preferred and proven by monitor mode.

    Shared by the air side (`wfb.manager`) and the ground side
    (`ground_station.wfb_rx`) so both halves choose identically.

    Selection order:

    1. If ``configured_iface`` (the ``video.wfb.interface`` override) is
       set, return it verbatim — the operator pinned it on purpose.
    2. Otherwise filter to adapters that are WFB-compatible AND advertise
       monitor mode, rank them RTL-family-first (EU before AU before any
       other passing chip) so bus order never decides, then iterate the
       ranked list trying ``set_monitor_fn`` on each. Return the FIRST
       interface that actually enters (and verifies, where the callback
       verifies) monitor mode.

    Returning None means no candidate could be proven injection-capable.
    A candidate that fails monitor-set is skipped, never retried here, so
    the caller's own backoff loop owns the retry cadence.
    """
    if configured_iface:
        return configured_iface

    compatible = [a for a in adapters if a.is_wfb_compatible and a.supports_monitor]
    if not compatible:
        return None

    ranked = sorted(compatible, key=_injection_rank)
    for adapter in ranked:
        iface = adapter.interface_name
        log.info(
            "wfb_adapter_candidate",
            interface=iface,
            chipset=adapter.chipset,
            rank=_injection_rank(adapter),
        )
        if set_monitor_fn(iface):
            log.info(
                "wfb_adapter_selected",
                interface=iface,
                chipset=adapter.chipset,
            )
            return iface
        log.warning(
            "wfb_adapter_monitor_rejected",
            interface=iface,
            chipset=adapter.chipset,
        )
    log.error("wfb_no_injection_adapter", candidates=len(compatible))
    return None


def set_monitor_mode(interface: str) -> bool:
    """Put a WiFi interface into monitor mode.

    Releases the interface from NetworkManager first (best-effort) so NM
    cannot hold it in managed mode or revert the switch, then brings the
    iface down, sets monitor mode, and brings it back up.

    Some RTL8812xx driver builds reject ``iw <iface> set monitor none``
    with EIO (-5) while accepting the older ``iw <iface> set type monitor``,
    so the type form is tried first and the flag form is the fallback. This
    keeps a dedicated RTL radio working on boards (e.g. Rockchip running
    NetworkManager) where the flag form fails. Returns True on success.
    """
    system = platform.system()
    if system != "Linux":
        log.warning("monitor_mode_unsupported", platform=system)
        return False

    _validate_interface_name(interface)

    # Never put the operator's control-path interface into monitor mode — that
    # downs it and severs the management session with no fallback. Defense in
    # depth: detect_wfb_adapters() already excludes it, but a config override
    # or a stale selection could still reach here.
    ctrl = control_interface()
    if ctrl and interface == ctrl:
        log.error("monitor_mode_refused_control_iface", interface=interface)
        return False

    # Release the radio from NetworkManager. A NM-managed interface can
    # refuse the monitor switch with EIO or silently revert it. Best-effort:
    # nmcli may be absent (minimal rootfs) or the iface already unmanaged.
    try:
        subprocess.run(
            ["nmcli", "dev", "set", interface, "managed", "no"],
            capture_output=True,
            text=True,
            timeout=10,
        )
    except (FileNotFoundError, subprocess.TimeoutExpired):
        pass

    def _run(cmd: list[str]) -> tuple[bool, str]:
        try:
            result = subprocess.run(cmd, capture_output=True, text=True, timeout=10)
            return result.returncode == 0, (result.stderr or "").strip()
        except FileNotFoundError:
            log.error("monitor_mode_tool_missing", cmd=cmd[0])
            return False, "tool missing"
        except subprocess.TimeoutExpired:
            log.error("monitor_mode_timeout", cmd=" ".join(cmd))
            return False, "timeout"

    # Bring the iface down before changing type — RTL drivers require it.
    ok, stderr = _run(["ip", "link", "set", interface, "down"])
    if not ok:
        log.error("monitor_mode_cmd_failed", cmd=f"ip link set {interface} down", stderr=stderr)
        return False

    # Set monitor mode. The type form works on RTL8812xx where the flag
    # form returns EIO; fall back to the flag form for any driver that
    # only accepts it.
    ok, stderr = _run(["iw", interface, "set", "type", "monitor"])
    if not ok:
        ok_fallback, stderr_fb = _run(["iw", interface, "set", "monitor", "none"])
        if not ok_fallback:
            log.error(
                "monitor_mode_cmd_failed",
                cmd=f"iw {interface} set type monitor (fallback: set monitor none)",
                stderr=f"type: {stderr} | none: {stderr_fb}",
            )
            return False

    ok, stderr = _run(["ip", "link", "set", interface, "up"])
    if not ok:
        log.error("monitor_mode_cmd_failed", cmd=f"ip link set {interface} up", stderr=stderr)
        return False

    # Force power-save off on the monitor interface so the radio never
    # parks and silently stalls wfb_tx/wfb_rx. Best-effort: a driver that
    # rejects the knob must not fail the monitor-mode bring-up, which has
    # already succeeded by this point.
    try:
        ps_result = subprocess.run(
            ["iw", "dev", interface, "set", "power_save", "off"],
            capture_output=True,
            text=True,
            timeout=10,
        )
        if ps_result.returncode != 0:
            log.warning(
                "monitor_mode_powersave_off_failed",
                interface=interface,
                stderr=ps_result.stderr.strip(),
            )
    except (FileNotFoundError, subprocess.TimeoutExpired) as exc:
        log.warning("monitor_mode_powersave_off_error", interface=interface, error=str(exc))

    log.info("monitor_mode_set", interface=interface)
    return True


def get_interface_mode(interface: str) -> str | None:
    """Return the interface operating mode ("monitor" | "managed" | ...).

    Reads `iw <iface> info`. Used to VERIFY that set_monitor_mode actually
    took effect: a managed-mode iface cannot inject, so wfb_tx on it produces
    zero frames even though every command "succeeded". Returns None when the
    mode cannot be read.
    """
    if platform.system() != "Linux":
        return None
    _validate_interface_name(interface)
    try:
        result = subprocess.run(
            ["iw", interface, "info"],
            capture_output=True,
            text=True,
            timeout=5,
        )
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return None
    if result.returncode != 0:
        return None
    for line in result.stdout.splitlines():
        line = line.strip()
        if line.startswith("type "):
            return line.split(None, 1)[1].strip()
    return None


def enabled_channels(interface: str) -> set[int]:
    """5 GHz channel numbers this adapter can actually use for the link.

    Parses `iw phy <phyN> channels` and keeps only channels that are not
    `(disabled)` and not radar/`no IR` (DFS channels need a CAC the link does
    not do). The drone and ground frequently run different regulatory domains,
    so the air channel must be in the intersection of both sides' enabled
    sets; this exposes the local half of that intersection. Empty set means
    "could not determine"; callers should treat that as "do not restrict".
    """
    if platform.system() != "Linux":
        return set()
    _validate_interface_name(interface)
    # interface -> wiphy index -> phyN
    try:
        info = subprocess.run(
            ["iw", interface, "info"], capture_output=True, text=True, timeout=5
        )
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return set()
    phy = None
    for line in info.stdout.splitlines():
        line = line.strip()
        if line.startswith("wiphy "):
            phy = f"phy{line.split()[1].strip()}"
            break
    if not phy:
        return set()
    try:
        chans = subprocess.run(
            ["iw", "phy", phy, "channels"], capture_output=True, text=True, timeout=8
        )
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return set()
    out: set[int] = set()
    import re as _re

    for line in chans.stdout.splitlines():
        # e.g. "* 5745 MHz [149]" (usable) / "* 5180 MHz [36] (disabled)" /
        #      "* 5260 MHz [52] (no IR, radar detection)"
        m = _re.search(r"\[(\d+)\]", line)
        if not m:
            continue
        low = line.lower()
        if "disabled" in low or "no ir" in low or "radar" in low:
            continue
        out.add(int(m.group(1)))
    return out


def set_regulatory_domain(domain: str) -> bool:
    """Apply a wifi regulatory domain via `iw reg set <domain>`.

    Optional, opt-in (config `video.wfb.reg_domain`). When the drone and
    ground run the same domain they enable the same channel set, which lets
    hopping use U-NII-1 where legal. Left unset by default; the home channel
    (149, U-NII-3) works without it. Best-effort: a failure is logged and the
    link still runs on whatever channels the kernel allows.
    """
    if platform.system() != "Linux":
        return False
    import re as _re

    dom = (domain or "").strip().upper()
    if not _re.fullmatch(r"[A-Z0-9]{2}", dom):
        log.warning("reg_domain_invalid", domain=domain)
        return False
    try:
        result = subprocess.run(
            ["iw", "reg", "set", dom], capture_output=True, text=True, timeout=5
        )
    except (FileNotFoundError, subprocess.TimeoutExpired):
        log.warning("reg_domain_tool_missing_or_timeout", domain=dom)
        return False
    if result.returncode != 0:
        log.warning("reg_domain_set_failed", domain=dom, stderr=result.stderr.strip())
        return False
    log.info("reg_domain_set", domain=dom)
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
