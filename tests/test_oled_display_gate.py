"""Tests for the OLED service's display-disabled self-exit gate.

Defense-in-depth alongside the systemd ConditionPathExists marker: when
/etc/ados/display.conf says display_id=none (no panel, or the boot probe
auto-reverted an unconfirmed overlay), the OLED service exits 0 cleanly
instead of probing for a panel that the installer already decided is absent.
"""

from __future__ import annotations

from pathlib import Path

import ados.services.ui.oled_service.service as oled_service


class TestDisplayConfDisabled:
    def test_missing_conf_is_not_disabled(self, monkeypatch, tmp_path: Path):
        monkeypatch.setattr(
            "ados.core.paths.DISPLAY_CONF_PATH", tmp_path / "absent.conf"
        )
        assert oled_service._display_conf_disabled() is False

    def test_display_id_none_is_disabled(self, monkeypatch, tmp_path: Path):
        conf = tmp_path / "display.conf"
        conf.write_text("display_id=none\nboard=rock-5c-lite\nhas_touch=false\n")
        monkeypatch.setattr("ados.core.paths.DISPLAY_CONF_PATH", conf)
        assert oled_service._display_conf_disabled() is True

    def test_display_id_panel_is_not_disabled(self, monkeypatch, tmp_path: Path):
        conf = tmp_path / "display.conf"
        conf.write_text(
            "display_id=waveshare35a\nboard=rock-5c-lite\nhas_touch=true\n"
        )
        monkeypatch.setattr("ados.core.paths.DISPLAY_CONF_PATH", conf)
        assert oled_service._display_conf_disabled() is False

    def test_commented_display_id_ignored(self, monkeypatch, tmp_path: Path):
        # A commented-out key must not be read as the live value.
        conf = tmp_path / "display.conf"
        conf.write_text("# display_id=none\ndisplay_id=waveshare35a\n")
        monkeypatch.setattr("ados.core.paths.DISPLAY_CONF_PATH", conf)
        assert oled_service._display_conf_disabled() is False
