"""Tests for `ados radio status` adapter-verdict rendering."""

from __future__ import annotations

from unittest.mock import patch

from click.testing import CliRunner

from ados.cli.radio import radio_group


def _invoke(status_payload: dict, bind_payload: dict | None = None) -> str:
    runner = CliRunner()

    def fake_request(_method: str, path: str, *_args, **_kwargs):
        # `radio status` now reads both the data-plane (/api/wfb) and the
        # bind-session snapshot (/api/wfb/pair/local-bind) so it can suppress
        # the transient "no injection radio" verdict during an active bind.
        if path == "/api/wfb/pair/local-bind":
            return (200, bind_payload or {})
        return (200, status_payload)

    with patch("ados.cli.radio._request", side_effect=fake_request):
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


def test_radio_status_suppresses_no_injection_during_active_bind():
    # During an in-flight bind the radio is torn down + rebuilt, so a false
    # "no injection radio" verdict is expected and must NOT be shown as an
    # error — the bind phase is shown instead.
    out = _invoke(
        {
            "state": "disconnected",
            "interface": "",
            "adapter": {"driver": "8812eu", "chipset": "RTL8812EU"},
            "adapter_chipset": "RTL8812EU",
            "adapter_injection_ok": False,
            "channel": 149,
        },
        bind_payload={"state": "waiting_peer", "active": True},
    )
    assert "Injection capable" in out
    assert "binding" in out
    assert "no injection radio" not in out
