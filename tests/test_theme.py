"""Tests for the dashboard theme palette and lazy primitive resolution.

Covers:

* `ados.services.ui.theme` palette lookup, fallback, and live-config
  driven `current_palette()`.
* `ados.services.ui.dashboards.components.primitives` legacy color
  constants resolving against the active palette via module
  `__getattr__`.
"""

from __future__ import annotations

import pytest

from ados.core import config as core_config
from ados.core.config import ADOSConfig
from ados.services.ui import theme as theme_mod
from ados.services.ui.dashboards.components import primitives
from ados.services.ui.theme import DARK, LIGHT, Palette, get_palette


class TestPaletteLookup:
    def test_dark_is_named_dark(self):
        assert DARK.name == "dark"

    def test_light_is_named_light(self):
        assert LIGHT.name == "light"

    def test_palette_is_frozen_dataclass(self):
        with pytest.raises(Exception):
            DARK.bg_primary = (1, 2, 3)  # type: ignore[misc]

    def test_get_palette_dark(self):
        assert get_palette("dark") is DARK

    def test_get_palette_light(self):
        assert get_palette("light") is LIGHT

    def test_get_palette_unknown_falls_back_to_dark(self):
        assert get_palette("solarized-mauve") is DARK

    def test_get_palette_empty_string_falls_back_to_dark(self):
        assert get_palette("") is DARK


class TestCurrentPalette:
    def test_default_config_resolves_to_dark(self, monkeypatch):
        # A freshly constructed ADOSConfig has ui.theme="dark", so
        # current_palette() must return DARK without raising.
        monkeypatch.setattr(core_config, "load_config", lambda *a, **kw: ADOSConfig())
        assert theme_mod.current_palette() is DARK

    def test_light_in_config_resolves_to_light(self, monkeypatch):
        cfg = ADOSConfig()
        cfg.ui.theme = "light"  # type: ignore[assignment]
        monkeypatch.setattr(core_config, "load_config", lambda *a, **kw: cfg)
        assert theme_mod.current_palette() is LIGHT

    def test_unknown_theme_in_config_falls_back_to_dark(self, monkeypatch):
        # We bypass the Pydantic validator by patching load_config so
        # the resolver still has to handle a stray value defensively.
        class _Cfg:
            class _UI:
                theme = "verdant"
            ui = _UI()

        monkeypatch.setattr(core_config, "load_config", lambda *a, **kw: _Cfg())
        # Falls back to dark and does not raise.
        assert theme_mod.current_palette() is DARK

    def test_load_config_failure_returns_dark(self, monkeypatch):
        def _boom(*a, **kw) -> ADOSConfig:
            raise RuntimeError("config disk gone")

        monkeypatch.setattr(core_config, "load_config", _boom)
        # Resolver must never raise into the render loop.
        assert theme_mod.current_palette() is DARK


class TestPrimitivesLazyColors:
    def _patch_palette(self, monkeypatch, palette: Palette) -> None:
        monkeypatch.setattr(primitives, "current_palette", lambda: palette)

    def test_bg_primary_resolves_to_active_palette(self, monkeypatch):
        self._patch_palette(monkeypatch, DARK)
        assert primitives.BG_PRIMARY == DARK.bg_primary
        self._patch_palette(monkeypatch, LIGHT)
        assert primitives.BG_PRIMARY == LIGHT.bg_primary

    def test_text_primary_resolves_to_active_palette(self, monkeypatch):
        self._patch_palette(monkeypatch, LIGHT)
        assert primitives.TEXT_PRIMARY == LIGHT.text_primary

    def test_status_success_resolves_to_active_palette(self, monkeypatch):
        self._patch_palette(monkeypatch, DARK)
        assert primitives.STATUS_SUCCESS == DARK.status_success

    def test_unknown_attribute_raises_attribute_error(self):
        with pytest.raises(AttributeError):
            _ = primitives.NOT_A_REAL_TOKEN  # type: ignore[attr-defined]

    def test_threshold_color_uses_active_palette(self, monkeypatch):
        self._patch_palette(monkeypatch, DARK)
        # value=90 with success_at=80, warning_at=60 (higher_is_better)
        # is success.
        assert (
            primitives.threshold_color(
                90.0, success_at=80, warning_at=60
            )
            == DARK.status_success
        )
        # None falls to text_tertiary on the active palette.
        assert (
            primitives.threshold_color(None, success_at=80, warning_at=60)
            == DARK.text_tertiary
        )

    def test_threshold_color_palette_override(self):
        # Explicit palette argument wins over the active palette.
        result = primitives.threshold_color(
            10.0,
            success_at=80,
            warning_at=60,
            palette=LIGHT,
        )
        assert result == LIGHT.status_error

    def test_threshold_color_lower_is_better(self):
        # value=30 with success_at=70, warning_at=85 (lower_is_better)
        # is success.
        assert (
            primitives.threshold_color(
                30.0,
                success_at=70,
                warning_at=85,
                direction="lower_is_better",
                palette=DARK,
            )
            == DARK.status_success
        )
        assert (
            primitives.threshold_color(
                75.0,
                success_at=70,
                warning_at=85,
                direction="lower_is_better",
                palette=DARK,
            )
            == DARK.status_warning
        )
        assert (
            primitives.threshold_color(
                90.0,
                success_at=70,
                warning_at=85,
                direction="lower_is_better",
                palette=DARK,
            )
            == DARK.status_error
        )


class TestConfigRoundTrip:
    def test_default_config_includes_ui_dark(self):
        cfg = ADOSConfig()
        assert cfg.ui.theme == "dark"

    def test_yaml_without_ui_section_loads_with_default(self, tmp_path, monkeypatch):
        from ados.core import config as cfg_mod

        yaml_text = "agent:\n  name: edge-rig\n"
        path = tmp_path / "config.yaml"
        path.write_text(yaml_text)
        # Make sure the loader picks our path and not the system one.
        cfg = cfg_mod.load_config(path)
        assert cfg.ui.theme == "dark"
        assert cfg.agent.name == "edge-rig"

    def test_yaml_with_ui_light_round_trips(self, tmp_path):
        from ados.core import config as cfg_mod

        yaml_text = "ui:\n  theme: light\n"
        path = tmp_path / "config.yaml"
        path.write_text(yaml_text)
        cfg = cfg_mod.load_config(path)
        assert cfg.ui.theme == "light"
