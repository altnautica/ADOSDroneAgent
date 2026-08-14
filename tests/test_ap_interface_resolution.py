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

import json
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


def test_the_captive_portal_ap_follows_the_onboard_chip(tmp_path, monkeypatch) -> None:
    # `WifiApManager._find_wireless_iface` used to return the FIRST `iw dev`
    # Interface line (readdir order) and fall back to a literal wlan0/wlan1, so
    # with the radio enumerating first it configured hostapd on the flight link.
    from ados.services.network import wifi_ap

    net = _sysfs(tmp_path, {"wlan0": RADIO, "wlan1": ONBOARD})
    monkeypatch.setattr(wifi_ap, "_NET_ROOT", net)
    mgr = wifi_ap.WifiApManager(device_id="abcd1234")
    assert mgr._find_wireless_iface() == "wlan1"


def test_the_captive_portal_ap_reports_no_wireless_when_the_box_has_none(
    tmp_path, monkeypatch
) -> None:
    # An empty sysfs resolves to the fallback name, which is absent here. The
    # caller's "no_wireless_interface" path must run rather than hostapd being
    # pointed at an interface that does not exist.
    from ados.services.network import wifi_ap

    net = tmp_path / "net"
    net.mkdir(parents=True)
    monkeypatch.setattr(wifi_ap, "_NET_ROOT", net)
    assert wifi_ap.WifiApManager()._find_wireless_iface() is None


def test_the_wifi_station_names_its_lock_after_the_radio_it_holds(
    tmp_path, monkeypatch
) -> None:
    # The station and the AP contend for one radio through an advisory lock, so
    # the station must resolve the same way the AP does AND name its lock after
    # what it resolved. A lock called "ados-wlan0.lock" held while the station
    # drives wlan1 excludes nobody, and hostapd walks onto the radio mid-join.
    from ados.services.ground_station import hostapd_manager, wifi_client_manager

    net = _sysfs(tmp_path, {"wlan0": RADIO, "wlan1": ONBOARD})
    monkeypatch.setattr(
        hostapd_manager,
        "resolve_ap_interface",
        lambda configured="", **kw: resolve_ap_interface(configured, net_root=net),
    )

    mgr = wifi_client_manager.WifiClientManager()
    assert mgr._interface == "wlan1", "the station must skip the flight radio too"
    assert mgr._lock_path.name == "ados-wlan1.lock"


def _wfb_stats(tmp_path: Path, interface: str) -> Path:
    """Build a `/run/ados` lookalike carrying the radio's own account."""
    run = tmp_path / "run"
    run.mkdir(parents=True, exist_ok=True)
    (run / "wfb-stats.json").write_text(json.dumps({"interface": interface}))
    return run


def test_the_radios_own_account_outranks_a_driver_table_that_missed_it(
    tmp_path,
) -> None:
    """The case a driver table cannot catch, and the reason the sidecar is read.

    Both interfaces look like ordinary onboard WiFi by driver string, so
    classification alone would hand the AP whichever sorted first. The radio
    reports it is holding that one. Binding hostapd to it would take the
    aircraft's link down, so the radio's account wins and the AP takes the
    other.
    """
    net = _sysfs(tmp_path, {"wlan0": ONBOARD, "wlan1": ONBOARD})
    run = _wfb_stats(tmp_path, "wlan0")
    assert resolve_ap_interface(net_root=net, run_root=run) == "wlan1"
    # Without the sidecar, classification alone picks the radio: this is the
    # assertion that proves the override is doing the work, not the ordering.
    assert resolve_ap_interface(net_root=net) == "wlan0"


def test_an_operator_pin_is_refused_when_the_radio_says_it_holds_it(
    tmp_path,
) -> None:
    net = _sysfs(tmp_path, {"wlan0": ONBOARD, "wlan1": ONBOARD})
    run = _wfb_stats(tmp_path, "wlan1")
    # The pin names the interface the radio is actually using, and the driver
    # table has no idea. Fall back rather than configure the AP on the link.
    assert resolve_ap_interface("wlan1", net_root=net, run_root=run) == "wlan0"


def test_a_missing_or_malformed_sidecar_leaves_classification_in_charge(
    tmp_path,
) -> None:
    net = _sysfs(tmp_path, {"wlan0": RADIO, "wlan1": ONBOARD})
    absent = tmp_path / "no-run"
    assert resolve_ap_interface(net_root=net, run_root=absent) == "wlan1"

    junk = tmp_path / "junk"
    junk.mkdir()
    (junk / "wfb-stats.json").write_text("not json at all")
    assert resolve_ap_interface(net_root=net, run_root=junk) == "wlan1"


def test_the_captive_portal_passes_the_operator_pin_to_the_same_resolver(
    tmp_path,
) -> None:
    """The AP manager and the hostapd manager must resolve identically.

    Dropping the pin on one side is how the two ended up naming different
    interfaces, which also gives them different advisory lock files and so
    excludes nobody.
    """
    from ados.services.network import wifi_ap

    net = _sysfs(tmp_path, {"wlan0": ONBOARD, "wlan1": ONBOARD})
    monkey = wifi_ap._NET_ROOT
    try:
        wifi_ap._NET_ROOT = net
        pinned = wifi_ap.WifiApManager(interface="wlan1")
        assert pinned._find_wireless_iface() == "wlan1"
        assert resolve_ap_interface("wlan1", net_root=net) == "wlan1"
    finally:
        wifi_ap._NET_ROOT = monkey
