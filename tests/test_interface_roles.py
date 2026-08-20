"""Tests for the single driver-keyed interface-role resolver.

Roles (wfb / mgmt_wifi / mesh) must be decided by the bound kernel driver,
never by the racing interface name. These tests build a `/sys/class/net`
lookalike and assert assign_roles picks the right radio regardless of the
order the interfaces happen to enumerate in.
"""

from __future__ import annotations

from pathlib import Path

from ados.services.network.interface_roles import (
    InterfaceRoles,
    assign_roles,
    driver_of,
    is_denied_management_driver,
    is_wfb_compatible_driver,
    normalize_driver,
    wfb_rank,
)

ONBOARD = "aic8800_fdrv"
ONBOARD2 = "brcmfmac"
RADIO = "rtl88x2eu"
RADIO2 = "8812au"


def _sysfs(tmp_path: Path, layout: dict[str, str]) -> Path:
    """Build a `/sys/class/net` lookalike from {iface: driver}."""
    net = tmp_path / "net"
    net.mkdir(parents=True)
    for iface, driver in layout.items():
        d = net / iface
        (d / "phy80211").mkdir(parents=True)
        target = tmp_path / "drivers" / driver
        target.mkdir(parents=True, exist_ok=True)
        (d / "device").mkdir()
        (d / "device" / "driver").symlink_to(target)
    return net


def test_normalize_and_classify_drivers():
    assert normalize_driver("RTL88X2EU") == "rtl88x2eu"
    assert is_wfb_compatible_driver("rtl88x2eu") is True
    assert is_wfb_compatible_driver("8812au") is True
    assert is_wfb_compatible_driver("aic8800_fdrv") is False
    assert is_denied_management_driver("aic8800_fdrv") is True
    assert is_denied_management_driver("brcmfmac") is True
    assert is_denied_management_driver("rtl88x2eu") is False


def test_wfb_rank_prefers_eu_then_au(tmp_path):
    assert wfb_rank("rtl88x2eu", "") < wfb_rank("8812au", "")
    # A known-but-other chip ranks after both.
    assert wfb_rank("8812au", "") < wfb_rank("someother", "someother")


def test_assign_roles_picks_wfb_by_rank_not_enumeration_order(tmp_path):
    # The dangerous ordering: the onboard management WiFi enumerates first.
    net = _sysfs(tmp_path, {"wlan0": ONBOARD, "wlan1": RADIO, "wlx2": ONBOARD2})
    roles = assign_roles(net_root=net)
    assert roles.wfb == "wlan1"
    assert roles.mgmt_wifi == "wlan0"
    assert roles.mesh == "wlx2"


def test_assign_roles_reversed_enumeration_still_same(tmp_path):
    # Reversed boot ordering must yield the identical roles (the ordering is
    # the bug). Both on-board chips are denied-management (equal priority), so
    # the name-sorted tiebreak decides between them; the flight radio's role
    # is unchanged regardless of how the box enumerates it.
    net = _sysfs(tmp_path, {"wlan0": ONBOARD2, "wlx1": RADIO, "wlan2": ONBOARD})
    roles = assign_roles(net_root=net)
    assert roles.wfb == "wlx1"
    assert roles.mgmt_wifi == "wlan0"
    assert roles.mesh == "wlan2"


def test_assign_roles_single_radio_no_mesh(tmp_path):
    net = _sysfs(tmp_path, {"wlan0": RADIO, "wlan1": ONBOARD})
    roles = assign_roles(net_root=net)
    assert roles == InterfaceRoles(wfb="wlan0", mgmt_wifi="wlan1", mesh=None)


def test_assign_roles_radio_only_has_no_control_takeover(tmp_path):
    # A box with ONLY the flight radio must not claim a control role for it.
    net = _sysfs(tmp_path, {"wlan0": RADIO})
    roles = assign_roles(net_root=net)
    assert roles.wfb == "wlan0"
    assert roles.mgmt_wifi is None
    assert roles.mesh is None


def test_assign_roles_accepts_explicit_interface_list(tmp_path):
    # Callers may pass an already-enumerated list; classification is unchanged.
    net = _sysfs(tmp_path, {"wlanA": RADIO, "wlanB": ONBOARD})
    roles = assign_roles(["wlanB", "wlanA"], net_root=net)
    assert roles.wfb == "wlanA"
    assert roles.mgmt_wifi == "wlanB"


def test_driver_of_reads_bound_kernel_module(tmp_path):
    net = _sysfs(tmp_path, {"wlan1": RADIO})
    assert driver_of("wlan1", net_root=net) == RADIO
    assert driver_of("missing", net_root=net) == ""
