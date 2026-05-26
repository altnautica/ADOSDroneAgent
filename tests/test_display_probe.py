"""Tests for the boot-time display presence probe (apply-verify-auto-revert).

The probe runs once per boot while a display overlay is on probation: a
boot-critical SPI-LCD overlay was applied before the panel could be confirmed
present. It either CONFIRMS the panel (clear probation, write the persistent
marker) or AUTO-REVERTS (restore the boot-config snapshot, disable the
display). These tests drive both verdicts against a mocked sysfs tree, and
verify the framebuffer-by-name matching that lets the panel be found on fb0
(headless) or fb1 (DRM owns fb0).
"""

from __future__ import annotations

from pathlib import Path

import pytest

from ados.services import display_probe


# A valid extlinux.conf is comfortably over the 100-byte sanity floor the
# revert path enforces before restoring a snapshot.
_GOOD_BOOT_CONFIG = (
    "LABEL ados\n"
    "  kernel /boot/vmlinuz\n"
    "  fdt /boot/board.dtb\n"
    "  append root=/dev/mmcblk0p2 rw rootwait console=ttyS2,1500000\n"
)
_EDITED_BOOT_CONFIG = _GOOD_BOOT_CONFIG + (
    "  fdtoverlays /boot/overlay-user/panel.dtbo  # ados:panel\n"
)


@pytest.fixture
def mock_tree(monkeypatch, tmp_path: Path):
    """Wire the probe's paths + sysfs roots to a temp tree."""
    etc = tmp_path / "etc" / "ados"
    boot = tmp_path / "boot" / "extlinux"
    etc.mkdir(parents=True)
    boot.mkdir(parents=True)
    sys_graphics = tmp_path / "sys" / "class" / "graphics"
    sys_input = tmp_path / "sys" / "class" / "input"
    sys_graphics.mkdir(parents=True)
    sys_input.mkdir(parents=True)

    probation = etc / "display.probation"
    enabled = etc / "display.enabled"
    conf = etc / "display.conf"
    boot_config = boot / "extlinux.conf"
    snapshot = boot / "extlinux.conf.ados-bak"

    monkeypatch.setattr(display_probe, "DISPLAY_PROBATION_PATH", probation)
    monkeypatch.setattr(display_probe, "DISPLAY_ENABLED_PATH", enabled)
    monkeypatch.setattr(display_probe, "DISPLAY_CONF_PATH", conf)
    monkeypatch.setattr(display_probe, "SYS_GRAPHICS_DIR", sys_graphics)
    monkeypatch.setattr(display_probe, "SYS_INPUT_DIR", sys_input)
    # Make the bind poll fast so the revert tests don't wait 20s.
    monkeypatch.setattr(display_probe, "_BIND_POLL_SECONDS", 0.2)
    monkeypatch.setattr(display_probe, "_BIND_POLL_INTERVAL", 0.05)

    return {
        "etc": etc,
        "probation": probation,
        "enabled": enabled,
        "conf": conf,
        "boot_config": boot_config,
        "snapshot": snapshot,
        "sys_graphics": sys_graphics,
        "sys_input": sys_input,
    }


def _arm_probation(t: dict, *, with_snapshot: bool = True) -> None:
    """Write a probation marker (+ optional restorable snapshot)."""
    if with_snapshot:
        t["snapshot"].write_text(_GOOD_BOOT_CONFIG)
        t["boot_config"].write_text(_EDITED_BOOT_CONFIG)
    t["probation"].write_text(
        "display_id=waveshare35a\n"
        "board=rock-5c-lite\n"
        f"snapshot={t['snapshot'] if with_snapshot else ''}\n"
        f"boot_config={t['boot_config']}\n"
        "expected_fb_name=fb_ili9486\n"
        "touch_chip=ADS7846\n"
    )


def _bind_panel(t: dict, fb_index: int, *, touch: bool = True) -> None:
    """Mock a bound fbtft framebuffer (+ optional ADS7846 touch input)."""
    fb = t["sys_graphics"] / f"fb{fb_index}"
    fb.mkdir(parents=True)
    (fb / "name").write_text("fb_ili9486\n")
    if touch:
        ev = t["sys_input"] / "event0" / "device"
        ev.mkdir(parents=True)
        (ev / "name").write_text("ADS7846 Touchscreen\n")


class TestConfirm:
    def test_panel_bound_on_fb1_confirms(self, mock_tree):
        t = mock_tree
        _arm_probation(t)
        _bind_panel(t, 1)

        rc = display_probe.run()
        assert rc == 0
        # Probation cleared, persistent marker written.
        assert not t["probation"].exists()
        assert t["enabled"].exists()
        # Boot config left as-is (overlay retained) — NOT reverted.
        assert t["boot_config"].read_text() == _EDITED_BOOT_CONFIG

    def test_panel_bound_on_fb0_confirms(self, mock_tree):
        """Headless rig: the SPI LCD lands on fb0 (no DRM claims it)."""
        t = mock_tree
        _arm_probation(t)
        _bind_panel(t, 0)

        rc = display_probe.run()
        assert rc == 0
        assert not t["probation"].exists()
        assert t["enabled"].exists()


class TestRevert:
    def test_no_panel_reverts_boot_config(self, mock_tree):
        t = mock_tree
        _arm_probation(t)
        # No framebuffer mocked -> panel never binds.

        rc = display_probe.run()
        assert rc == 0
        # Boot config restored to the pristine snapshot (overlay line gone).
        assert t["boot_config"].read_text() == _GOOD_BOOT_CONFIG
        # display.conf disabled, marker removed, probation cleared.
        assert "display_id=none" in t["conf"].read_text()
        assert not t["enabled"].exists()
        assert not t["probation"].exists()

    def test_fb_without_touch_reverts(self, mock_tree):
        """A framebuffer carrying the fbtft name but with NO touch device is
        not a confirmed panel (the second signal is required) -> revert."""
        t = mock_tree
        _arm_probation(t)
        _bind_panel(t, 0, touch=False)

        rc = display_probe.run()
        assert rc == 0
        assert t["boot_config"].read_text() == _GOOD_BOOT_CONFIG
        assert not t["probation"].exists()

    def test_revert_refuses_tiny_snapshot(self, mock_tree):
        """A truncated snapshot must never be restored over a working config."""
        t = mock_tree
        t["snapshot"].write_text("x\n")  # < 100 bytes
        t["boot_config"].write_text(_EDITED_BOOT_CONFIG)
        t["probation"].write_text(
            "display_id=waveshare35a\n"
            f"snapshot={t['snapshot']}\n"
            f"boot_config={t['boot_config']}\n"
            "expected_fb_name=fb_ili9486\n"
            "touch_chip=ADS7846\n"
        )

        rc = display_probe.run()
        assert rc == 0
        # The edited config is left untouched (we did NOT clobber it with a
        # bad snapshot); display still disabled + probation cleared.
        assert t["boot_config"].read_text() == _EDITED_BOOT_CONFIG
        assert "display_id=none" in t["conf"].read_text()
        assert not t["probation"].exists()

    def test_no_snapshot_still_disables(self, mock_tree):
        """No snapshot recorded: cannot revert the boot config, but still
        disables the display + clears probation (best-effort safe subset)."""
        t = mock_tree
        _arm_probation(t, with_snapshot=False)

        rc = display_probe.run()
        assert rc == 0
        assert "display_id=none" in t["conf"].read_text()
        assert not t["enabled"].exists()
        assert not t["probation"].exists()


class TestNoOp:
    def test_no_probation_is_noop(self, mock_tree):
        t = mock_tree
        # No probation marker armed.
        rc = display_probe.run()
        assert rc == 0
        # Nothing written.
        assert not t["enabled"].exists()
        assert not t["conf"].exists()


class TestFbByName:
    def test_fb_bound_prefers_expected_name(self, mock_tree):
        t = mock_tree
        # fb0 is a non-SPI HDMI framebuffer, fb1 is the SPI LCD.
        (t["sys_graphics"] / "fb0").mkdir()
        (t["sys_graphics"] / "fb0" / "name").write_text("rockchip-drm\n")
        (t["sys_graphics"] / "fb1").mkdir()
        (t["sys_graphics"] / "fb1" / "name").write_text("fb_ili9486\n")
        assert display_probe._fb_bound("fb_ili9486") == "fb1"

    def test_fb_bound_falls_back_to_known_driver(self, mock_tree):
        t = mock_tree
        (t["sys_graphics"] / "fb0").mkdir()
        (t["sys_graphics"] / "fb0" / "name").write_text("fb_st7789v\n")
        # Expected name not present, but a known SPI-LCD driver is.
        assert display_probe._fb_bound("fb_ili9486") == "fb0"

    def test_fb_bound_ignores_non_spi(self, mock_tree):
        t = mock_tree
        (t["sys_graphics"] / "fb0").mkdir()
        (t["sys_graphics"] / "fb0" / "name").write_text("BCM2708 FB\n")
        assert display_probe._fb_bound("fb_ili9486") is None
