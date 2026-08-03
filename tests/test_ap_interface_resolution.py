"""The access point must find its radio by driver, not by name.

Measured on a ground station across three reboots: `wlan0` was the onboard
Broadcom chip on two of them and the USB RTL8812EU flight radio on the third.
The AP bound a hardcoded `wlan0`, so on the unlucky boot it would have
configured hostapd on the aircraft's radio link -- while the module docstring
asserted the RTL "is never touched here", which was true only when the naming
happened to fall the right way.

Both orderings are exercised deliberately, because the ordering IS the bug.
Mirrors the Rust twin in `ados_protocol::netif`.
"""

from __future__ import annotations

from pathlib import Path

from ados.services.ground_station.hostapd_manager import resolve_ap_interface

ONBOARD = "brcmfmac"
RADIO = "rtl88x2eu"


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


def test_the_ap_takes_the_onboard_chip(tmp_path) -> None:
    net = _sysfs(tmp_path, {"wlan0": ONBOARD, "wlan1": RADIO})
    assert resolve_ap_interface(net_root=net) == "wlan0"


def test_when_the_radio_takes_wlan0_the_ap_follows_the_onboard_chip(tmp_path) -> None:
    # The exact boot that would have put hostapd on the flight radio.
    net = _sysfs(tmp_path, {"wlan0": RADIO, "wlan1": ONBOARD})
    assert resolve_ap_interface(net_root=net) == "wlan1"


def test_a_configured_radio_interface_is_refused(tmp_path) -> None:
    net = _sysfs(tmp_path, {"wlan0": ONBOARD, "wlan1": RADIO})
    # An operator pinning the flight radio is not honoured. It falls back, and
    # the AP's own start path refuses separately if the fallback is also the
    # radio -- so no single wrong answer can take the aircraft's link.
    assert resolve_ap_interface("wlan1", net_root=net) != "wlan1"


def test_an_operator_pin_on_a_safe_interface_wins(tmp_path) -> None:
    net = _sysfs(tmp_path, {"wlan0": ONBOARD, "wlan1": ONBOARD})
    assert resolve_ap_interface("wlan1", net_root=net) == "wlan1"


def test_a_box_with_only_the_radio_does_not_steal_it(tmp_path) -> None:
    net = _sysfs(tmp_path, {"wlan0": RADIO})
    # Nothing onboard to use. It reports the fallback, and the start path then
    # refuses because the fallback IS the radio. Running the access point on
    # the flight link is never the better outcome.
    assert resolve_ap_interface(net_root=net) == "wlan0"


def test_an_unreadable_sysfs_falls_back_rather_than_raising(tmp_path) -> None:
    # A dev host has no /sys/class/net. Resolution degrades instead of throwing.
    assert resolve_ap_interface(net_root=tmp_path / "absent") == "wlan0"
