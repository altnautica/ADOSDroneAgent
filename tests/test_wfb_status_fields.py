"""Tests for the /api/wfb adapter introspection fallback.

Asserts the adapter introspection fills in driver / chipset /
supports_monitor from sysfs + iw, and that the driver-to-chipset mapping
resolves the known RTL families. The transmit and receive status-field
contracts are now owned by the native radio services and covered by their
own tests.
"""

from __future__ import annotations

from unittest.mock import MagicMock, patch


def test_api_introspect_adapter_fills_driver_and_monitor():
    from ados.api.routes import wfb as wfb_routes

    fake_proc = MagicMock()
    fake_proc.returncode = 0
    fake_proc.stdout = "Supported interface modes:\n\t\t * monitor\n"

    with patch(
        "ados.api.routes.wfb.os.readlink",
        return_value="/sys/bus/usb/drivers/rtl88x2eu",
    ), patch(
        "ados.api.routes.wfb.Path"
    ) as path_mock, patch(
        "ados.api.routes.wfb.subprocess.run", return_value=fake_proc
    ):
        path_mock.return_value.read_text.return_value = "1\n"
        info = wfb_routes._introspect_adapter("wlan0")

    assert info["driver"] == "rtl88x2eu"
    assert info["chipset"] == "RTL8812EU"
    assert info["supports_monitor"] is True


def test_api_introspect_adapter_empty_for_blank_iface():
    from ados.api.routes import wfb as wfb_routes

    info = wfb_routes._introspect_adapter("")
    assert info == {
        "driver": "",
        "chipset": "",
        "supports_monitor": False,
    }


def test_chipset_from_driver_mapping():
    from ados.api.routes.wfb import _chipset_from_driver

    assert _chipset_from_driver("rtl88x2eu") == "RTL8812EU"
    assert _chipset_from_driver("8812au") == "RTL8812AU"
    assert _chipset_from_driver("88XXau") == "RTL8812AU"
    assert _chipset_from_driver("somethingelse") == "somethingelse"
    assert _chipset_from_driver("") == ""


def test_base_block_carries_no_fabricated_adapter_verdict():
    """The config-seeded base is what a caller sees when the radio service
    never wrote a sidecar, i.e. no adapter scan has produced a verdict. The
    injection verdict must read None there: a False is a fabricated
    measured no-injection claim that three-state consumers (CLI, GCS)
    render as a red scan outcome for hardware nothing ever examined.
    """
    from ados.api.routes.wfb import _base_block

    base = _base_block(None)
    assert base["adapter_injection_ok"] is None
    # The USB / PHY verdicts must not be seeded at all (absent reads as
    # "no reading" downstream); seeding a boolean would be the same lie.
    assert "adapter_usb_degraded" not in base
    assert "phy_muted" not in base
