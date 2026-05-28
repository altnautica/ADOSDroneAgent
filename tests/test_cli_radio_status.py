"""Tests for `ados radio status` adapter-verdict rendering."""

from __future__ import annotations

from unittest.mock import patch

from click.testing import CliRunner

from ados.cli.radio import radio_group


def _invoke(status_payload: dict) -> str:
    runner = CliRunner()
    with patch(
        "ados.cli.radio._request",
        return_value=(200, status_payload),
    ):
        result = runner.invoke(radio_group, ["status"])
    return result.output


def test_radio_status_shows_selected_radio_and_injection_ok():
    out = _invoke(
        {
            "state": "connected",
            "interface": "wlan1",
            "adapter": {"driver": "8812eu", "chipset": "RTL8812EU"},
            "adapter_chipset": "RTL8812EU",
            "adapter_injection_ok": True,
            "channel": 149,
        }
    )
    assert "Selected radio" in out
    assert "RTL8812EU" in out
    assert "Injection capable" in out
    assert "yes" in out


def test_radio_status_shows_loud_no_injection_verdict():
    out = _invoke(
        {
            "state": "disconnected",
            "interface": "",
            "adapter": {"driver": "aic8800_fdrv", "chipset": "management-wifi"},
            "adapter_chipset": None,
            "adapter_injection_ok": False,
            "channel": 149,
        }
    )
    assert "Injection capable" in out
    assert "NO" in out
    assert "no injection radio" in out
