"""Tests for the shared RTL-preferred, try-and-fail WFB adapter selector.

Covers the management-WiFi deny-set, RTL-first ranking (order
independence), the monitor-mode try-and-fail loop, and the loud
no-injection-adapter failure surfaced on the heartbeat.
"""

from __future__ import annotations

from ados.services.wfb.adapter import (
    WifiAdapterInfo,
    _injection_rank,
    _is_denied_management_wifi,
    select_wfb_interface,
)


def _adapter(
    name: str,
    *,
    chipset: str,
    driver: str = "",
    vid: int = 0,
    pid: int = 0,
    compat: bool = True,
    monitor: bool = True,
) -> WifiAdapterInfo:
    return WifiAdapterInfo(
        interface_name=name,
        driver=driver or chipset.lower(),
        chipset=chipset,
        supports_monitor=monitor,
        current_mode="managed",
        usb_vid=vid,
        usb_pid=pid,
        is_wfb_compatible=compat,
    )


# --- deny-set ---


def test_aic8800_denied_by_vendor():
    assert _is_denied_management_wifi(0xA69C, "anything") is True


def test_aic8800_denied_by_driver_prefix():
    # Driver suffixes vary (aic8800_fdrv, aic8800d80_usb) — prefix match.
    assert _is_denied_management_wifi(0, "aic8800_fdrv") is True
    assert _is_denied_management_wifi(0, "aic8800d80_usb") is True


def test_brcmfmac_denied_by_driver():
    assert _is_denied_management_wifi(0, "brcmfmac") is True


def test_rtl_not_denied():
    assert _is_denied_management_wifi(0x0BDA, "8812eu") is False
    assert _is_denied_management_wifi(0x0BDA, "rtl88x2eu") is False


# --- detect_wfb_adapters: AIC8800 / brcmfmac never compatible ---


def _patch_linux_scan(monkeypatch, iw_dev, iw_phy, driver_map, usb_id_map):
    from unittest.mock import MagicMock

    from ados.services.wfb import adapter as adapter_mod

    monkeypatch.setattr(adapter_mod.platform, "system", lambda: "Linux")

    def _run(cmd, **_kw):
        result = MagicMock()
        result.returncode = 0
        result.stdout = ""
        if cmd[:2] == ["iw", "dev"]:
            result.stdout = iw_dev
        elif cmd[:2] == ["iw", "phy"]:
            result.stdout = iw_phy
        elif cmd[0] == "readlink":
            for frag, drv in driver_map.items():
                if frag in cmd[1]:
                    result.stdout = f"/sys/bus/usb/drivers/{drv}\n"
                    break
        return result

    monkeypatch.setattr(adapter_mod.subprocess, "run", _run)
    monkeypatch.setattr(adapter_mod, "discover_usb_devices", lambda: [])
    monkeypatch.setattr(
        adapter_mod, "_get_usb_id_for_interface", lambda iface: usb_id_map.get(iface, (0, 0))
    )


def test_detect_aic8800_never_compatible_even_if_vid_resolves(monkeypatch):
    """An AIC8800 management WiFi must never be tagged compatible, even
    if its USB walk happens to resolve an idVendor and it advertises
    monitor mode — the deny gate runs before any compat path."""
    from ados.services.wfb.adapter import detect_wfb_adapters

    iw_dev = (
        "phy#0\n\tInterface wlan0\n\t\ttype managed\n"
        "phy#1\n\tInterface wlan1\n\t\ttype managed\n"
    )
    iw_phy = (
        "Wiphy phy0\n\tSupported interface modes:\n\t\t * managed\n\t\t * monitor\n"
        "Wiphy phy1\n\tSupported interface modes:\n\t\t * managed\n\t\t * monitor\n"
    )
    _patch_linux_scan(
        monkeypatch,
        iw_dev,
        iw_phy,
        driver_map={"wlan0": "aic8800_fdrv", "wlan1": "8812eu"},
        usb_id_map={"wlan0": (0xA69C, 0x8800), "wlan1": (0x0BDA, 0xA81A)},
    )

    by_name = {a.interface_name: a for a in detect_wfb_adapters()}
    assert by_name["wlan0"].is_wfb_compatible is False
    assert by_name["wlan1"].is_wfb_compatible is True


def test_detect_brcmfmac_never_compatible(monkeypatch):
    from ados.services.wfb.adapter import detect_wfb_adapters

    iw_dev = "phy#0\n\tInterface wlan0\n\t\ttype managed\n"
    iw_phy = "Wiphy phy0\n\tSupported interface modes:\n\t\t * managed\n\t\t * monitor\n"
    _patch_linux_scan(
        monkeypatch,
        iw_dev,
        iw_phy,
        driver_map={"wlan0": "brcmfmac"},
        usb_id_map={},
    )
    adapters = detect_wfb_adapters()
    assert adapters[0].is_wfb_compatible is False


# --- ranking ---


def test_rtl_ranks_ahead_of_other():
    rtl_eu = _adapter("wlan1", chipset="RTL8812EU", driver="8812eu", vid=0x0BDA, pid=0xB812)
    rtl_au = _adapter("wlan2", chipset="RTL8812AU", driver="8812au", vid=0x0BDA, pid=0x8812)
    other = _adapter("wlan3", chipset="MysteryChip", driver="mystery")
    assert _injection_rank(rtl_eu) < _injection_rank(rtl_au) < _injection_rank(other)


# --- select_wfb_interface ---


def test_select_prefers_rtl_with_aic_first(monkeypatch):
    """AIC8800 listed FIRST, RTL second — RTL must still be chosen.

    (The AIC8800 is denied at detect time, so it would not normally reach
    the selector with is_wfb_compatible=True; this asserts the ranking +
    try-and-fail also rejects a non-injection adapter that slipped the
    filter and was enumerated first on the bus.)"""
    aic = _adapter("wlan0", chipset="management-wifi", driver="aic8800_fdrv")
    rtl = _adapter("wlan1", chipset="RTL8812EU", driver="8812eu", vid=0x0BDA, pid=0xB812)

    monitored: list[str] = []

    def _set_monitor(iface: str) -> bool:
        monitored.append(iface)
        # The AIC8800 accepts the commands but cannot actually enter
        # monitor mode; model that as a failed set so try-and-fail skips it.
        return iface != "wlan0"

    # AIC first, RTL second.
    chosen = select_wfb_interface([aic, rtl], _set_monitor, "")
    assert chosen == "wlan1"
    # Ranking floats RTL to the front, so the AIC is never even tried.
    assert monitored == ["wlan1"]


def test_select_order_independent_aic_first_and_second(monkeypatch):
    """RTL chosen regardless of where the AIC8800 sits in bus order."""
    aic = _adapter("wlan0", chipset="management-wifi", driver="aic8800_fdrv")
    rtl = _adapter("wlan9", chipset="RTL8812EU", driver="8812eu", vid=0x0BDA, pid=0xB812)

    def _set_monitor(iface: str) -> bool:
        return iface == "wlan9"

    # AIC first.
    assert select_wfb_interface([aic, rtl], _set_monitor, "") == "wlan9"
    # AIC second.
    assert select_wfb_interface([rtl, aic], _set_monitor, "") == "wlan9"


def test_select_try_and_fail_skips_failing_candidate():
    """A ranked candidate that fails monitor-set is skipped in favor of
    the RTL that does accept it."""
    # Two RTLs ranked equally-ish; the first one fails monitor-set.
    rtl_a = _adapter("wlan0", chipset="RTL8812AU", driver="8812au", vid=0x0BDA, pid=0x8812)
    rtl_b = _adapter("wlan1", chipset="RTL8812EU", driver="8812eu", vid=0x0BDA, pid=0xB812)

    def _set_monitor(iface: str) -> bool:
        # wlan0 (AU) is ranked after wlan1 (EU); but to prove the skip we
        # make the FIRST-ranked one (EU=wlan1) fail and assert fall-through.
        return iface == "wlan0"

    chosen = select_wfb_interface([rtl_a, rtl_b], _set_monitor, "")
    assert chosen == "wlan0"


def test_select_no_injection_adapter_returns_none():
    """No compatible adapter at all → None (the loud-fail trigger)."""
    aic = _adapter("wlan0", chipset="management-wifi", compat=False)
    assert select_wfb_interface([aic], lambda i: True, "") is None


def test_select_all_candidates_fail_monitor_returns_none():
    """Compatible adapters present but none enters monitor → None."""
    rtl = _adapter("wlan1", chipset="RTL8812EU", driver="8812eu", vid=0x0BDA, pid=0xB812)
    assert select_wfb_interface([rtl], lambda i: False, "") is None


def test_select_config_override_wins():
    """A configured iface is returned verbatim, no probing."""
    rtl = _adapter("wlan1", chipset="RTL8812EU", driver="8812eu", vid=0x0BDA, pid=0xB812)
    calls: list[str] = []

    def _set_monitor(iface: str) -> bool:
        calls.append(iface)
        return True

    chosen = select_wfb_interface([rtl], _set_monitor, "wlan_override")
    assert chosen == "wlan_override"
    # Override path must not probe any adapter.
    assert calls == []
