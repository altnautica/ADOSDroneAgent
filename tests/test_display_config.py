"""Tests for the HDMI groundnode primary-path display config + heartbeat.

Covers:

* ``GroundStationConfig.display`` default shape (auto / detected_type=None).
* The OLED ``_amain`` early-exit gate honours ``display.type == "hdmi"``
  and ``display.type == "none"`` by returning 0 before any I2C or
  framebuffer probe runs.
* Cloud heartbeat ``build_display_type_enrichment`` forwards explicit
  selections verbatim and probes HDMI / LCD presence under ``"auto"``.
"""

from __future__ import annotations

from pathlib import Path
from unittest.mock import patch

import pytest

from ados.core.config import ADOSConfig
from ados.core.config.ground_station import DisplayConfig, GroundStationConfig
from ados.services.cloud import heartbeat as cloud_heartbeat
from ados.services.ui.oled_service import service as oled_service

# --- DisplayConfig defaults ------------------------------------------------


def test_display_config_defaults() -> None:
    """Fresh GroundStationConfig has display.type='auto' and no detected_type."""
    gs = GroundStationConfig()
    assert isinstance(gs.display, DisplayConfig)
    assert gs.display.type == "auto"
    assert gs.display.detected_type is None


def test_display_config_each_instance_is_fresh() -> None:
    """default_factory gives each instance its own DisplayConfig (no shared mutable)."""
    a = GroundStationConfig()
    b = GroundStationConfig()
    assert a.display is not b.display


def test_display_config_round_trips_through_root_model() -> None:
    """The root ADOSConfig exposes ground_station.display with the defaults."""
    cfg = ADOSConfig()
    assert cfg.ground_station.display.type == "auto"
    assert cfg.ground_station.display.detected_type is None


@pytest.mark.parametrize("kind", ["auto", "hdmi", "lcd", "none"])
def test_display_config_accepts_all_literal_values(kind: str) -> None:
    """All four DisplayType literals validate."""
    dc = DisplayConfig(type=kind)  # type: ignore[arg-type]
    assert dc.type == kind


# --- OLED early-startup gate -----------------------------------------------


def _config_with_display(kind: str) -> ADOSConfig:
    cfg = ADOSConfig()
    cfg.ground_station.display.type = kind  # type: ignore[assignment]
    return cfg


def _run_oled_amain(monkeypatch: pytest.MonkeyPatch, cfg: ADOSConfig) -> int:
    """Drive ``oled_service._amain`` with a stubbed ButtonEventBus + OledService.

    The gate runs before the bus or the service is built, so for the
    skip cases we never hit those imports. For the not-skipped cases we
    stub them so we exercise the path without spinning up real
    hardware.
    """
    import asyncio

    monkeypatch.setattr(oled_service, "load_config", lambda: cfg)
    monkeypatch.setattr(oled_service, "configure_logging", lambda _level: None)

    class _StubBus:
        async def close(self) -> None:
            return None

    class _StubService:
        def __init__(self, *_args, **_kwargs) -> None:
            self._stop = None

        def request_stop(self) -> None:
            return None

        def request_reload(self) -> None:
            return None

        async def run(self) -> int:
            return 0

    monkeypatch.setattr(oled_service, "ButtonEventBus", _StubBus)
    monkeypatch.setattr(oled_service, "OledService", _StubService)

    return asyncio.run(oled_service._amain())


def test_oled_skips_when_display_type_hdmi(
    monkeypatch: pytest.MonkeyPatch, caplog: pytest.LogCaptureFixture
) -> None:
    """display.type='hdmi' returns 0 without constructing the bus / service."""
    cfg = _config_with_display("hdmi")

    called = {"bus": False, "service": False}

    def _bus_factory() -> object:
        called["bus"] = True
        raise AssertionError("ButtonEventBus must not be built when gated")

    def _service_factory(*_args, **_kwargs) -> object:
        called["service"] = True
        raise AssertionError("OledService must not be built when gated")

    monkeypatch.setattr(oled_service, "load_config", lambda: cfg)
    monkeypatch.setattr(oled_service, "configure_logging", lambda _level: None)
    monkeypatch.setattr(oled_service, "ButtonEventBus", _bus_factory)
    monkeypatch.setattr(oled_service, "OledService", _service_factory)

    import asyncio
    rc = asyncio.run(oled_service._amain())

    assert rc == 0
    assert called == {"bus": False, "service": False}


def test_oled_skips_when_display_type_none(monkeypatch: pytest.MonkeyPatch) -> None:
    """display.type='none' returns 0 without constructing the bus / service."""
    cfg = _config_with_display("none")

    def _explode(*_args, **_kwargs) -> object:
        raise AssertionError("must not be built when display.type=none")

    monkeypatch.setattr(oled_service, "load_config", lambda: cfg)
    monkeypatch.setattr(oled_service, "configure_logging", lambda _level: None)
    monkeypatch.setattr(oled_service, "ButtonEventBus", _explode)
    monkeypatch.setattr(oled_service, "OledService", _explode)

    import asyncio
    rc = asyncio.run(oled_service._amain())
    assert rc == 0


@pytest.mark.parametrize("kind", ["auto", "lcd"])
def test_oled_runs_when_display_type_auto_or_lcd(
    monkeypatch: pytest.MonkeyPatch, kind: str
) -> None:
    """display.type='auto' or 'lcd' falls through to bus + service construction."""
    cfg = _config_with_display(kind)
    rc = _run_oled_amain(monkeypatch, cfg)
    assert rc == 0  # the stub service returns 0


# --- Heartbeat displayType enrichment --------------------------------------


@pytest.mark.parametrize("kind", ["hdmi", "lcd", "none"])
def test_build_display_type_enrichment_explicit(kind: str) -> None:
    """Explicit selections are forwarded verbatim, no probing."""
    cfg = ADOSConfig()
    cfg.ground_station.display.type = kind  # type: ignore[assignment]
    # Even if HDMI is present in the environment we must not override
    # the operator's explicit choice — assert by mocking hdmi_present
    # to True and verifying the lcd / none selection still wins.
    with patch(
        "ados.services.kiosk.kiosk_service.hdmi_present", return_value=True
    ):
        out = cloud_heartbeat.build_display_type_enrichment(cfg)
    assert out == {"displayType": kind}


def test_build_display_type_enrichment_auto_with_hdmi_present(
    tmp_path: Path,
) -> None:
    """auto + HDMI present → 'hdmi' (HDMI wins even if LCD is also wired)."""
    display_conf = tmp_path / "display.conf"
    display_conf.write_text(
        "framebuffer_path=/dev/fb1\n"
        "display_id=waveshare35a\n"
    )
    cfg = ADOSConfig()
    assert cfg.ground_station.display.type == "auto"
    with patch(
        "ados.services.kiosk.kiosk_service.hdmi_present", return_value=True
    ), patch.object(cloud_heartbeat, "DISPLAY_CONF_PATH", display_conf):
        out = cloud_heartbeat.build_display_type_enrichment(cfg)
    assert out == {"displayType": "hdmi"}


def test_build_display_type_enrichment_auto_with_lcd_only(tmp_path: Path) -> None:
    """auto + no HDMI + display.conf present → 'lcd'."""
    display_conf = tmp_path / "display.conf"
    display_conf.write_text(
        "framebuffer_path=/dev/fb1\n"
        "display_id=waveshare35a\n"
    )
    cfg = ADOSConfig()
    with patch(
        "ados.services.kiosk.kiosk_service.hdmi_present", return_value=False
    ), patch.object(cloud_heartbeat, "DISPLAY_CONF_PATH", display_conf):
        out = cloud_heartbeat.build_display_type_enrichment(cfg)
    assert out == {"displayType": "lcd"}


def test_build_display_type_enrichment_auto_no_displays(tmp_path: Path) -> None:
    """auto + no HDMI + no display.conf → 'none'."""
    missing = tmp_path / "does-not-exist.conf"
    cfg = ADOSConfig()
    with patch(
        "ados.services.kiosk.kiosk_service.hdmi_present", return_value=False
    ), patch.object(cloud_heartbeat, "DISPLAY_CONF_PATH", missing):
        out = cloud_heartbeat.build_display_type_enrichment(cfg)
    assert out == {"displayType": "none"}


def test_build_display_type_enrichment_handles_missing_block() -> None:
    """A defective config (no ground_station.display) falls back to 'auto' probing."""

    class _Stub:
        pass

    stub = _Stub()
    stub.ground_station = _Stub()  # type: ignore[attr-defined]
    # No display attribute on ground_station.
    with patch(
        "ados.services.kiosk.kiosk_service.hdmi_present", return_value=False
    ), patch.object(
        cloud_heartbeat, "DISPLAY_CONF_PATH", Path("/nonexistent/display.conf")
    ):
        out = cloud_heartbeat.build_display_type_enrichment(stub)
    assert out == {"displayType": "none"}


# --- Kiosk public alias ----------------------------------------------------


def test_kiosk_hdmi_present_public_alias_matches_private() -> None:
    """``hdmi_present`` and ``_hdmi_present`` return the same value."""
    from ados.services.kiosk import kiosk_service as ks

    assert ks.hdmi_present() == ks._hdmi_present()
