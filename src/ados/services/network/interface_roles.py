"""Deterministic interface-role assignment by driver / USB identity.

Roles (``wfb``, ``mgmt_wifi``, ``mesh``) are decided by the bound kernel driver
and the WFB compat/deny sets — NEVER by kernel interface name. Interface names
race at boot (measured: ``wlan0`` was the onboard chip twice and the USB WFB
flight radio once across three reboots), so every subsystem that owns a radio —
WFB-ng injection selection, the management-WiFi client/AP, and mesh — consults
this ONE resolver and a kernel name never decides a role.

Classification rules (all by driver):

* ``wfb``: the highest-ranked WFB-compatible radio (RTL8812EU before
  RTL8812AU before any other passing chip) per the same ranking the radio
  service uses for injection.
* ``mgmt_wifi``: the first managed wireless that is NOT a WFB radio,
  preferring the denied on-board management chips (AIC8800, brcmfmac) — the
  chips the WFB injector refuses, i.e. exactly the radios an operator wants
  carrying their control WiFi.
* ``mesh``: the next such non-WFB wireless (a second control-class radio),
  when one exists.

The deny-prefix set is the "management-WiFi driver-prefix deny-set": chips
that advertise monitor mode but cannot inject, so they are *denied* from the
WFB role and thereby *selected* for the control/mgmt role.

The driver sets live in ``ados.services.wfb._wfb_tables_generated`` (generated
from ``crates/ados-protocol/wfb-adapters.toml``) — the single source of truth
shared with the Rust radio service. This module never forks that data.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

from ados.services.wfb._wfb_tables_generated import (
    WFB_COMPATIBLE_DRIVERS,
    WFB_DENY_DRIVER_PREFIXES,
)

__all__ = [
    "InterfaceRoles",
    "assign_roles",
    "driver_of",
    "is_denied_management_driver",
    "is_wfb_compatible_driver",
    "normalize_driver",
    "wfb_rank",
    "wireless_interfaces",
]


def normalize_driver(driver: str) -> str:
    """Lowercase, stripped driver name (or ``""`` when unknown)."""
    return (driver or "").strip().lower()


def is_wfb_compatible_driver(driver: str) -> bool:
    """True when ``driver`` is one of the WFB-compatible kernel modules."""
    return normalize_driver(driver) in {d.lower() for d in WFB_COMPATIBLE_DRIVERS}


def is_denied_management_driver(driver: str) -> bool:
    """True when ``driver`` is a denied management-WiFi chip (can't inject).

    The deny-prefix set tolerates the ``_fdrv`` / ``_usb`` suffixes those
    chips bind under, so a prefix match is used.
    """
    d = normalize_driver(driver)
    return any(d.startswith(p) for p in WFB_DENY_DRIVER_PREFIXES)


def driver_of(iface: str, net_root: Path | None = None) -> str:
    """Read the kernel driver bound to ``iface``, or ``""`` when unreadable.

    ``net_root`` is injectable for tests (defaults to ``/sys/class/net``).
    """
    root = net_root or Path("/sys/class/net")
    link = root / iface / "device" / "driver"
    if not link.exists():
        return ""
    try:
        return link.resolve().name
    except OSError:
        return ""


def wireless_interfaces(net_root: Path | None = None) -> list[str]:
    """All wireless netdev names under ``net_root`` (sorted, stable order)."""
    root = net_root or Path("/sys/class/net")
    try:
        return sorted(
            p.name
            for p in root.iterdir()
            if (p / "phy80211").exists() or (p / "wireless").exists()
        )
    except OSError:
        return []


def wfb_rank(
    driver: str = "",
    chipset: str = "",
    usb_vid: int = 0,
    usb_pid: int = 0,
) -> int:
    """Injection preference rank, lower is better.

    Mirrors the radio service's ranking: RTL8812EU silicon first, RTL8812AU
    rebadges next, any other chip that merely passed the compat filter last.
    Independent of USB bus order so a management WiFi enumerated first never
    wins by accident.
    """
    from ados.services.wfb._wfb_tables_generated import WFB_COMPATIBLE

    label = (chipset or "").upper()
    drv = normalize_driver(driver)
    is_eu = (
        "8812EU" in label
        or "88X2EU" in label
        or drv in {"8812eu", "rtl8812eu", "rtl88x2eu"}
    )
    is_au = (
        "8812AU" in label
        or "88XXAU" in label
        or drv in {"8812au", "rtl8812au", "rtl88xxau"}
    )
    is_known = (
        (usb_vid, usb_pid) in WFB_COMPATIBLE or drv in {d.lower() for d in WFB_COMPATIBLE_DRIVERS}
    )
    if is_eu:
        return 0
    if is_au:
        return 1
    if is_known:
        return 2
    return 3


@dataclass(frozen=True)
class InterfaceRoles:
    """One interface per radio role, resolved by driver.

    Any field may be ``None`` when no adapter on the box satisfies that role.
    """

    wfb: str | None
    mgmt_wifi: str | None
    mesh: str | None


def assign_roles(
    interfaces: list[str] | None = None,
    *,
    net_root: Path | None = None,
) -> InterfaceRoles:
    """Assign every wireless adapter a role, deterministically, by driver.

    ``interfaces`` (optional) overrides the sysfs enumeration (for tests /
    callers that already have the list). Each role is a *distinct* interface;
    the same physical radio is never claimed by two roles.
    """
    root = net_root or Path("/sys/class/net")
    ifaces = sorted(set(interfaces or wireless_interfaces(root)))
    drivers = {i: driver_of(i, root) for i in ifaces}

    # wfb: highest-ranked WFB-compatible radio.
    compat = [i for i in ifaces if is_wfb_compatible_driver(drivers[i])]
    compat.sort(key=lambda i: wfb_rank(drivers[i]))
    wfb = compat[0] if compat else None

    # Non-WFB radios are the control/mgmt/mesh pool. Prefer the denied
    # on-board management chips first, then any other non-WFB wireless.
    non_wfb = [i for i in ifaces if not is_wfb_compatible_driver(drivers[i])]
    non_wfb.sort(
        key=lambda i: (
            0 if is_denied_management_driver(drivers[i]) else 1,
            i,
        )
    )
    mgmt_wifi = non_wfb[0] if non_wfb else None
    mesh = non_wfb[1] if len(non_wfb) > 1 else None

    return InterfaceRoles(wfb=wfb, mgmt_wifi=mgmt_wifi, mesh=mesh)
