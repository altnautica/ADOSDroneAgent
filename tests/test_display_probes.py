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

        # Redirect Path("/dev") to an empty tmp dir so glob("fb*") finds
        # nothing, simulating a board where the overlay has been written
        # to disk but the kernel hasn't bound the panel yet.
        empty_dev = tmp_path / "dev_empty"
        empty_dev.mkdir()
        original_path_cls = hc.Path

        def _patched(*args, **kwargs):
            obj = original_path_cls(*args, **kwargs)
            if str(obj) == "/dev":
                return original_path_cls(empty_dev)
            return obj

        monkeypatch.setattr(hc, "Path", _patched)

        item = hc._check_display()
        assert item.state == "warning"
        assert "no SPI LCD framebuffer is bound" in item.detail
        assert "reboot" in (item.fix_hint or "").lower()

    def test_conf_present_fb_bound_correct_driver_returns_ok(
        self, monkeypatch, tmp_path: Path
    ):
        from ados.setup import hardware_check as hc

        conf = tmp_path / "display.conf"
        conf.write_text(
            "display_id=waveshare35a\n"
            "framebuffer_path=/dev/fb1\n"
            "framebuffer_name_expected=fb_ili9486\n"
            "has_touch=true\n"
            "resolution=480x320\n"
            "rotation=90\n"
        )
        monkeypatch.setattr("ados.core.paths.DISPLAY_CONF_PATH", conf)

        # Spin up a fake /dev with one fb1 file, plus a fake
        # /sys/class/graphics/fb1/name reporting the expected driver.
        fake_dev = tmp_path / "dev_with_fb1"
        fake_dev.mkdir()
        (fake_dev / "fb1").write_bytes(b"")
        sys_class = tmp_path / "sys_graphics"
        (sys_class / "fb1").mkdir(parents=True)
        (sys_class / "fb1" / "name").write_text("fb_ili9486\n")

        original_path_cls = hc.Path

        def _patched(*args, **kwargs):
            obj = original_path_cls(*args, **kwargs)
            if str(obj) == "/dev":
                return original_path_cls(fake_dev)
            sys_str = str(obj)
            if sys_str.startswith("/sys/class/graphics/"):
                tail = sys_str.removeprefix("/sys/class/graphics/")
                return original_path_cls(sys_class) / tail
            return obj

        monkeypatch.setattr(hc, "Path", _patched)

        item = hc._check_display()
        assert item.state == "ok", f"unexpected state: {item.state} | {item.detail}"
        assert "waveshare35a" in item.detail
        assert "+ touch" in item.detail
        assert "480x320" in item.detail
        assert "fb1" in item.detail

    def test_pi_4b_no_hdmi_panel_takes_fb0(
        self, monkeypatch, tmp_path: Path
    ):
        """Pi 4B without an HDMI monitor: bcm2708_fb disables itself,
        the SPI LCD takes /dev/fb0 instead of /dev/fb1. The probe must
        find the panel anyway by walking all /dev/fb* and matching by
        driver name in /sys/class/graphics/<fbN>/name."""
        from ados.setup import hardware_check as hc

        conf = tmp_path / "display.conf"
        # Note: framebuffer_path here is the install-time hint, not a
        # constraint on where the probe looks at runtime.
        conf.write_text(
            "display_id=waveshare35a\n"
            "framebuffer_path=/dev/fb1\n"
            "framebuffer_name_expected=fb_ili9486\n"
            "has_touch=true\n"
            "resolution=480x320\n"
            "rotation=90\n"
        )
        monkeypatch.setattr("ados.core.paths.DISPLAY_CONF_PATH", conf)

        fake_dev = tmp_path / "dev_with_fb0"
        fake_dev.mkdir()
        (fake_dev / "fb0").write_bytes(b"")
        sys_class = tmp_path / "sys_graphics"
        (sys_class / "fb0").mkdir(parents=True)
        (sys_class / "fb0" / "name").write_text("fb_ili9486\n")

        original_path_cls = hc.Path

        def _patched(*args, **kwargs):
            obj = original_path_cls(*args, **kwargs)
            if str(obj) == "/dev":
                return original_path_cls(fake_dev)
            sys_str = str(obj)
            if sys_str.startswith("/sys/class/graphics/"):
                tail = sys_str.removeprefix("/sys/class/graphics/")
                return original_path_cls(sys_class) / tail
            return obj

        monkeypatch.setattr(hc, "Path", _patched)

        item = hc._check_display()
        assert item.state == "ok", f"unexpected state: {item.state} | {item.detail}"
        # The probe must report the actual fb index, not the staged one.
        assert "fb0" in item.detail
        assert "fb_ili9486" in item.detail

    def test_unknown_driver_on_only_fb_returns_warning(
        self, monkeypatch, tmp_path: Path
    ):
        """A framebuffer is bound but its driver is not in the known
        SPI-LCD set. Probe must NOT mistakenly tag it as an SPI LCD."""
        from ados.setup import hardware_check as hc

        conf = tmp_path / "display.conf"
        conf.write_text(
            "display_id=waveshare35a\n"
            "framebuffer_path=/dev/fb0\n"
            "framebuffer_name_expected=fb_ili9486\n"
        )
        monkeypatch.setattr("ados.core.paths.DISPLAY_CONF_PATH", conf)

        fake_dev = tmp_path / "dev_with_hdmi_fb0"
        fake_dev.mkdir()
        (fake_dev / "fb0").write_bytes(b"")
        sys_class = tmp_path / "sys_graphics"
        (sys_class / "fb0").mkdir(parents=True)
        # bcm2708_fb is the standard Pi HDMI driver, not an SPI LCD.
        (sys_class / "fb0" / "name").write_text("BCM2708 FB\n")

        original_path_cls = hc.Path

        def _patched(*args, **kwargs):
            obj = original_path_cls(*args, **kwargs)
            if str(obj) == "/dev":
                return original_path_cls(fake_dev)
            sys_str = str(obj)
            if sys_str.startswith("/sys/class/graphics/"):
                tail = sys_str.removeprefix("/sys/class/graphics/")
                return original_path_cls(sys_class) / tail
            return obj

        monkeypatch.setattr(hc, "Path", _patched)

        item = hc._check_display()
        assert item.state == "warning"
        assert "no SPI LCD framebuffer is bound" in item.detail


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
