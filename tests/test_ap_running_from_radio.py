"""`HostapdManager.status()["running"]` must mean "an SSID is on the air".

It used to mean `systemctl is-active ados-hostapd`. That unit ran the manager
itself, which parks in an idle sleep when the operator has not opted the hotspot
in precisely so systemd keeps the unit `active` — so every ground station with
the hotspot switched off reported a broadcasting access point, and the Rust
ground-station status routes copied the value verbatim.

The derivation is now the radio's own account: nl80211 `type AP` plus corroboration
that START_AP actually completed (an operating channel) or that a station is
associated.
"""

from __future__ import annotations

import pytest

import ados.services.ground_station.hostapd_manager as hm
from ados.core.subprocess import CmdResult


def _iw_info(*, iface_type: str, channel: int | None, ssid: str = "ADOS-GS-AB12") -> str:
    """`iw dev wlan0 info` output, in the layout iw actually prints."""
    lines = [
        "Interface wlan0",
        "\tifindex 3",
        "\twdev 0x1",
        "\taddr dc:a6:32:00:00:01",
        f"\tssid {ssid}",
        f"\ttype {iface_type}",
        "\twiphy 0",
    ]
    if channel is not None:
        lines.append(
            f"\tchannel {channel} (2437 MHz), width: 20 MHz, center1: 2437 MHz"
        )
    lines.append("\ttxpower 31.00 dBm")
    return "\n".join(lines) + "\n"


def _station_dump(*macs: str) -> str:
    out = []
    for mac in macs:
        out.append(f"Station {mac} (on wlan0)")
        out.append("\tinactive time:\t120 ms")
        out.append("\trx bytes:\t4096")
    return "\n".join(out) + "\n" if out else ""


@pytest.fixture
def radio(monkeypatch):
    """Stub the three commands `status()` can run, and record what it asked.

    Hardware-free and deterministic: every answer is fixed by the test, and the
    call log is what proves which question the derivation actually asked.
    """

    state = {
        "unit_active": True,
        "info": _iw_info(iface_type="AP", channel=6),
        "info_error": None,
        "stations": "",
        "calls": [],
    }

    def _run(cmd, **kwargs):
        cmd = list(cmd)
        state["calls"].append(cmd)
        if cmd[:2] == ["systemctl", "is-active"]:
            body = "active\n" if state["unit_active"] else "inactive\n"
            return CmdResult(returncode=0 if state["unit_active"] else 3, stdout=body, stderr="")
        if cmd[:2] == ["iw", "dev"] and cmd[3] == "info":
            if state["info_error"] is not None:
                raise state["info_error"]
            return CmdResult(returncode=0, stdout=state["info"], stderr="")
        if cmd[:2] == ["iw", "dev"] and cmd[3] == "station":
            return CmdResult(returncode=0, stdout=state["stations"], stderr="")
        raise AssertionError(f"unexpected command: {cmd}")

    monkeypatch.setattr(hm, "run_cmd_sync", _run)
    return state


def _manager() -> hm.HostapdManager:
    return hm.HostapdManager(device_id="ab12cd34", interface="wlan0")


def test_an_active_unit_with_no_ap_on_the_radio_is_not_running(radio) -> None:
    # The regression, exactly: unit `active`, radio in managed mode because the
    # onboard chip is being used as a WiFi client. Note the managed interface
    # also reports an operating channel, which is why `type` is load-bearing.
    radio["unit_active"] = True
    radio["info"] = _iw_info(iface_type="managed", channel=6, ssid="SomeHomeNet")

    status = _manager().status()

    assert status["running"] is False
    assert status["hostapd_unit_active"] is True
    assert status["radio"]["iface_type"] == "managed"
    assert status["radio"]["beaconing"] is False


def test_the_upstream_ap_is_never_counted_as_a_client(radio) -> None:
    # `station dump` on a managed interface lists the AP this box JOINED. Asking
    # would report the network it is a client of as a client of its own.
    radio["info"] = _iw_info(iface_type="managed", channel=6)
    radio["stations"] = _station_dump("aa:bb:cc:dd:ee:ff")

    status = _manager().status()

    assert status["connected_clients"] == []
    assert status["radio"]["station_count"] == 0
    assert not any(
        c[:2] == ["iw", "dev"] and c[3] == "station" for c in radio["calls"]
    ), "a managed interface must not be asked for stations"


def test_ap_mode_with_a_started_bss_is_running(radio) -> None:
    radio["unit_active"] = False  # systemd's view is not consulted for `running`
    radio["info"] = _iw_info(iface_type="AP", channel=6)

    status = _manager().status()

    assert status["running"] is True
    assert status["hostapd_unit_active"] is False
    assert status["radio"]["operating_channel"] == 6
    assert status["radio"]["ssid"] == "ADOS-GS-AB12"


def test_ap_mode_before_start_ap_completes_is_not_running(radio) -> None:
    # Interface put into AP mode but no operating channel: hostapd is up and the
    # driver has not accepted START_AP, so nothing is beaconing yet.
    radio["info"] = _iw_info(iface_type="AP", channel=None)

    status = _manager().status()

    assert status["running"] is False
    assert status["radio"]["iface_type"] == "AP"
    assert status["radio"]["operating_channel"] is None


def test_an_associated_station_proves_an_ap_even_without_a_channel_line(radio) -> None:
    radio["info"] = _iw_info(iface_type="AP", channel=None)
    radio["stations"] = _station_dump("aa:bb:cc:dd:ee:ff", "11:22:33:44:55:66")

    status = _manager().status()

    assert status["running"] is True
    assert status["connected_clients"] == ["aa:bb:cc:dd:ee:ff", "11:22:33:44:55:66"]
    assert status["radio"]["station_count"] == 2


def test_an_unaskable_radio_is_reported_as_unknown_not_as_an_ap(radio) -> None:
    # No `iw`, or the interface is gone. "Could not ask" must be distinguishable
    # from "asked, and there is no AP" — the Rust status routes branch on it.
    radio["info_error"] = FileNotFoundError("iw")

    status = _manager().status()

    assert status["running"] is False
    assert status["radio"]["probe_ok"] is False
    assert status["radio"]["iface_type"] is None


def test_a_hostile_ssid_cannot_forge_the_type_or_channel_fields(radio) -> None:
    radio["info"] = _iw_info(iface_type="managed", channel=None, ssid="type AP channel 6")

    status = _manager().status()

    assert status["running"] is False
    assert status["radio"]["iface_type"] == "managed"
    assert status["radio"]["operating_channel"] is None
