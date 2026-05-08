"""Tests for the display-presence probes consumed by the hardware-check
wizard step and the cloud heartbeat assembler.

The probes read /etc/ados/display.conf and /sys/class/graphics; both
are mocked here to exercise every state the wizard surfaces without
touching the real filesystem.
"""

from __future__ import annotations

from pathlib import Path

import pytest

# ---------------------------------------------------------------------------
# hardware_check._check_display() — wizard-facing probe
# ---------------------------------------------------------------------------


class TestCheckDisplay:
    def test_no_conf_returns_unknown(self, monkeypatch, tmp_path: Path):
        from ados.setup import hardware_check as hc

        bogus = tmp_path / "missing.conf"
        # Patch the constant the function imports.
        monkeypatch.setattr("ados.core.paths.DISPLAY_CONF_PATH", bogus)

        item = hc._check_display()
        assert item.id == "display"
        assert item.state == "unknown"
        assert "No /etc/ados/display.conf" in item.detail

    def test_conf_present_fb_absent_returns_warning(
        self, monkeypatch, tmp_path: Path
    ):
        from ados.setup import hardware_check as hc

        conf = tmp_path / "display.conf"
        conf.write_text(
            "display_id=waveshare35a\n"
            "framebuffer_path=/dev/fb_does_not_exist\n"
            "framebuffer_name_expected=fb_ili9486\n"
            "has_touch=true\n"
            "resolution=480x320\n"
            "rotation=90\n"
        )
        monkeypatch.setattr("ados.core.paths.DISPLAY_CONF_PATH", conf)

        item = hc._check_display()
        assert item.state == "warning"
        assert "Reboot to load the overlay." in item.detail
        assert "reboot" in (item.fix_hint or "").lower()

    def test_conf_present_fb_bound_correct_driver_returns_ok(
        self, monkeypatch, tmp_path: Path
    ):
        from ados.setup import hardware_check as hc

        conf = tmp_path / "display.conf"
        conf.write_text(
            "display_id=waveshare35a\n"
            "framebuffer_path=/dev/fb_fake\n"
            "framebuffer_name_expected=fb_ili9486\n"
            "has_touch=true\n"
            "resolution=480x320\n"
            "rotation=90\n"
        )
        # Create a fake fb device file.
        fb = tmp_path / "fb_fake"
        fb.write_bytes(b"")
        # Patch DISPLAY_CONF_PATH and override Path("/sys/class/graphics")
        # via the function-local Path import. Easier: monkeypatch the
        # framebuffer_path lookup by writing a fake /sys tree.
        sys_class = tmp_path / "sys_graphics"
        (sys_class / "fb_fake").mkdir(parents=True)
        (sys_class / "fb_fake" / "name").write_text("fb_ili9486\n")

        # The function builds Path("/sys/class/graphics") / fb_path.name.
        # Redirect by patching Path inside the module to a small wrapper.
        # Cleaner: edit the conf to point at a temp path that the function
        # can stat — combined with a /sys lookup we shim by monkeypatching
        # Path resolution.
        original_path_cls = hc.Path

        def _patched(*args, **kwargs):
            obj = original_path_cls(*args, **kwargs)
            # Redirect /sys/class/graphics/<x>/name to our temp shim.
            sys_str = str(obj)
            if sys_str.startswith("/sys/class/graphics/"):
                tail = sys_str.removeprefix("/sys/class/graphics/")
                return original_path_cls(sys_class) / tail
            # Redirect /dev/fb_fake to our temp file.
            if sys_str == "/dev/fb_fake":
                return fb
            return obj

        # `Path` is referenced as `Path(...)` inside the function. Patch
        # the module-level attribute.
        monkeypatch.setattr(hc, "Path", _patched)
        monkeypatch.setattr("ados.core.paths.DISPLAY_CONF_PATH", conf)

        item = hc._check_display()
        assert item.state == "ok", f"unexpected state: {item.state} | {item.detail}"
        assert "waveshare35a" in item.detail
        assert "+ touch" in item.detail
        assert "480x320" in item.detail


# ---------------------------------------------------------------------------
# cloud._collect_attached_display() — heartbeat-facing assembler
# ---------------------------------------------------------------------------


class TestCollectAttachedDisplay:
    def test_no_conf_returns_none(self, monkeypatch, tmp_path: Path):
        from ados.services.cloud import heartbeat as cloud_main

        bogus = tmp_path / "absent.conf"
        monkeypatch.setattr(cloud_main, "DISPLAY_CONF_PATH", bogus)
        assert cloud_main.collect_attached_display() is None

    def test_conf_present_returns_peripheral_dict(
        self, monkeypatch, tmp_path: Path
    ):
        from ados.services.cloud import heartbeat as cloud_main

        conf = tmp_path / "display.conf"
        conf.write_text(
            "display_id=waveshare35a\n"
            "board=cubie-a7z\n"
            "controller=ILI9486\n"
            "touch_chip=ADS7846\n"
            "has_touch=true\n"
            "resolution=480x320\n"
            "framebuffer_path=/dev/fb_test_fake_does_not_exist\n"
            "framebuffer_name_expected=fb_ili9486\n"
            "rotation=90\n"
            "overlay_source=repo\n"
            "overlay_ref=cubie-a7z-waveshare35a.dts\n"
            "activated_via=extlinux\n"
        )
        monkeypatch.setattr(cloud_main, "DISPLAY_CONF_PATH", conf)

        result = cloud_main.collect_attached_display()
        assert result is not None
        assert result["category"] == "display"
        assert result["type"] == "spi-lcd"
        assert result["id"] == "local-display"
        assert result["name"] == 'Waveshare 3.5" SPI LCD'
        assert result["address"] == "/dev/fb_test_fake_does_not_exist"
        # No /sys binding for the fake fb path -> bound=False, status="warning"
        assert result["status"] == "warning"
        assert result["extra"]["controller"] == "ILI9486"
        assert result["extra"]["has_touch"] is True
        assert result["extra"]["resolution"] == "480x320"
        assert result["extra"]["rotation"] == 90
        assert result["extra"]["board"] == "cubie-a7z"
        assert result["extra"]["overlay_source"] == "repo"
        assert result["extra"]["activated_via"] == "extlinux"
        assert result["extra"]["bound"] is False

    def test_unknown_display_id_falls_through_to_id_string(
        self, monkeypatch, tmp_path: Path
    ):
        from ados.services.cloud import heartbeat as cloud_main

        conf = tmp_path / "display.conf"
        conf.write_text("display_id=mystery_panel_v1\n")
        monkeypatch.setattr(cloud_main, "DISPLAY_CONF_PATH", conf)
        result = cloud_main.collect_attached_display()
        assert result is not None
        assert result["name"] == "mystery_panel_v1"

    def test_empty_conf_returns_none(self, monkeypatch, tmp_path: Path):
        from ados.services.cloud import heartbeat as cloud_main

        conf = tmp_path / "display.conf"
        conf.write_text("# only a comment\n\n")
        monkeypatch.setattr(cloud_main, "DISPLAY_CONF_PATH", conf)
        assert cloud_main.collect_attached_display() is None
