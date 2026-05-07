"""Tests for the AP-side mDNS announcer.

The zeroconf library is mocked at the symbol level so the test does
not bind a real socket. iface_ip is also patched so the test works on
hosts where wlan0 does not exist.
"""

from __future__ import annotations

from unittest.mock import MagicMock

import pytest

from ados.services.ground_station import mdns_announce
from ados.services.ground_station.mdns_announce import APAnnouncer


def _patched_zeroconf(monkeypatch: pytest.MonkeyPatch) -> tuple[MagicMock, MagicMock]:
    """Replace `from zeroconf import ServiceInfo, Zeroconf` with mocks."""
    zc_class = MagicMock(name="Zeroconf")
    info_class = MagicMock(name="ServiceInfo")
    fake_zeroconf = MagicMock()
    fake_zeroconf.Zeroconf = zc_class
    fake_zeroconf.ServiceInfo = info_class
    monkeypatch.setitem(__import__("sys").modules, "zeroconf", fake_zeroconf)
    return zc_class, info_class


def test_announcer_registers_when_iface_holds_expected_ip(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    zc_class, info_class = _patched_zeroconf(monkeypatch)
    monkeypatch.setattr(mdns_announce, "iface_ip", lambda iface: "192.168.4.1")

    ann = APAnnouncer(
        port=8080,
        device_id="abcd1234",
        version="1.2.3",
        iface="wlan0",
    )
    assert ann.is_ap_up() is True

    ok = ann.start()
    assert ok is True
    zc_instance = zc_class.return_value
    zc_instance.register_service.assert_called_once()

    # Verify TXT record values reach the ServiceInfo constructor.
    args, kwargs = info_class.call_args
    properties = kwargs["properties"]
    assert properties[b"profile"] == b"ground_station"
    assert properties[b"version"] == b"1.2.3"
    assert properties[b"device_id"] == b"abcd1234"
    assert properties[b"path"] == b"/api/v1/ground-station"
    assert kwargs["port"] == 8080
    assert kwargs["type_"] == "_ados._tcp.local."

    ann.stop()
    zc_instance.unregister_service.assert_called_once()
    zc_instance.close.assert_called_once()


def test_announcer_skips_when_iface_unassigned(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _patched_zeroconf(monkeypatch)
    monkeypatch.setattr(mdns_announce, "iface_ip", lambda iface: None)

    ann = APAnnouncer(port=8080, device_id="x", version="0", iface="wlan0")
    assert ann.is_ap_up() is False
    assert ann.start() is False


def test_announcer_skips_when_iface_holds_other_ip(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _patched_zeroconf(monkeypatch)
    monkeypatch.setattr(mdns_announce, "iface_ip", lambda iface: "10.0.0.5")

    ann = APAnnouncer(port=8080, device_id="x", version="0", iface="wlan0")
    assert ann.is_ap_up() is False
    assert ann.start() is False


def test_stop_is_idempotent_when_never_started() -> None:
    ann = APAnnouncer(port=8080, device_id="x", version="0", iface="wlan0")
    # Should not raise even though start() was never called.
    ann.stop()
    ann.stop()


def test_announcer_unregisters_on_register_failure(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    zc_class, _ = _patched_zeroconf(monkeypatch)
    monkeypatch.setattr(mdns_announce, "iface_ip", lambda iface: "192.168.4.1")
    zc_class.return_value.register_service.side_effect = RuntimeError("boom")

    ann = APAnnouncer(port=8080, device_id="x", version="0", iface="wlan0")
    assert ann.start() is False
    zc_class.return_value.close.assert_called_once()
