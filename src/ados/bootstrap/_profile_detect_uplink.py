"""Network uplink probes for the boot-time profile auto-detect.

Re-exported from ``ados.bootstrap.profile_detect``.
"""

from __future__ import annotations

from pathlib import Path

# Predictable network interface naming covers a few common patterns. The
# kernel + udev path on most BSPs lands in one of:
#   * eth*  — legacy "ethN" path on Pi-class boards and many BSPs.
#   * end*  — Rockchip/Radxa BSP DT-driven names (e.g. Rock 5C "end1").
#   * enp*  — systemd-predictable PCI slot names ("enp5s0", "enp0s31f6").
#   * enx*  — systemd-predictable MAC-derived names ("enx001122334455"),
#             used by USB-Ethernet adapters.
# Patterns are probed in priority order, with "up" beating "merely-present"
# across patterns.
_ETHERNET_IFACE_PATTERNS = ("eth*", "end*", "enp*", "enx*")


def _detect_ethernet_iface() -> str | None:
    """Return the most likely active ethernet interface name.

    Probes /sys/class/net for common patterns in priority order:
    eth*, end*, enp*, enx*. Within each pattern, prefer the first
    interface with operstate=up; fall back to the first that exists.
    Returns None if no ethernet-like interface is present.

    The constructor uses ``Path("/sys/class/net")`` so tests can
    monkeypatch the ``Path`` symbol in this module to reroute the
    sysfs reads under a tmp tree.
    """
    net_dir = Path("/sys/class/net")

    # First pass: find any iface with operstate=up, in pattern priority order.
    for pattern in _ETHERNET_IFACE_PATTERNS:
        try:
            candidates = sorted(net_dir.glob(pattern), key=lambda p: p.name)
        except OSError:
            candidates = []
        for iface_dir in candidates:
            operstate_path = iface_dir / "operstate"
            try:
                if (
                    operstate_path.is_file()
                    and operstate_path.read_text().strip().lower() == "up"
                ):
                    return iface_dir.name
            except OSError:
                # Race: device disappeared between glob and read. Skip it.
                continue

    # Second pass: nothing was up, return the first existing iface so a
    # caller that wants to know the iface name (e.g. for nmcli) still has
    # something to work with.
    for pattern in _ETHERNET_IFACE_PATTERNS:
        try:
            candidates = sorted(net_dir.glob(pattern), key=lambda p: p.name)
        except OSError:
            candidates = []
        for iface_dir in candidates:
            try:
                if iface_dir.exists():
                    return iface_dir.name
            except OSError:
                continue

    return None


def probe_uplink_type() -> tuple[int, int, bool]:
    """Check for any active uplink: ethernet, USB tether, or WiFi.

    Returns (ground_score, air_score, detected). The score columns
    are unchanged from the original scoring contract; only the third
    boolean is consumed by the hardware-check uplink probe, and it
    now also flips to True when wlan* is up.
    """
    detected = False

    eth_iface = _detect_ethernet_iface()
    if eth_iface is not None:
        eth_carrier = Path(f"/sys/class/net/{eth_iface}/carrier")
        try:
            if eth_carrier.is_file():
                if eth_carrier.read_text().strip() == "1":
                    detected = True
        except OSError:
            pass
        if not detected:
            # Some drivers leave carrier unreadable on link-down; fall back
            # to operstate so we don't mistakenly mark the link silent.
            eth_operstate = Path(f"/sys/class/net/{eth_iface}/operstate")
            try:
                if (
                    eth_operstate.is_file()
                    and eth_operstate.read_text().strip().lower() == "up"
                ):
                    detected = True
            except OSError:
                pass

    if not detected:
        usb_state = Path("/sys/class/net/usb0/operstate")
        try:
            if usb_state.is_file():
                if usb_state.read_text().strip().lower() == "up":
                    detected = True
        except OSError:
            pass

    if not detected:
        try:
            for wlan_dir in sorted(Path("/sys/class/net").glob("wlan*")):
                op = wlan_dir / "operstate"
                if op.is_file() and op.read_text().strip().lower() == "up":
                    detected = True
                    break
        except OSError:
            pass

    return (1, 0, True) if detected else (0, 0, False)


def probe_uplink_kinds() -> list[str]:
    """Return a list of active uplink kinds: ``ethernet``, ``usb-tether``, ``wifi``.

    Empty list when nothing is up. Used by the hardware-check uplink
    probe so it can show ``Active via WiFi`` instead of a generic
    ``ethernet/USB`` blob.
    """
    kinds: list[str] = []
    eth_iface = _detect_ethernet_iface()
    if eth_iface is not None:
        eth_link_up = False
        try:
            eth = Path(f"/sys/class/net/{eth_iface}/carrier")
            if eth.is_file() and eth.read_text().strip() == "1":
                eth_link_up = True
        except OSError:
            pass
        if not eth_link_up:
            try:
                op = Path(f"/sys/class/net/{eth_iface}/operstate")
                if op.is_file() and op.read_text().strip().lower() == "up":
                    eth_link_up = True
            except OSError:
                pass
        if eth_link_up:
            kinds.append("ethernet")
    try:
        usb = Path("/sys/class/net/usb0/operstate")
        if usb.is_file() and usb.read_text().strip().lower() == "up":
            kinds.append("USB tether")
    except OSError:
        pass
    try:
        for wlan_dir in sorted(Path("/sys/class/net").glob("wlan*")):
            op = wlan_dir / "operstate"
            if op.is_file() and op.read_text().strip().lower() == "up":
                kinds.append("WiFi")
                break
    except OSError:
        pass
    return kinds
