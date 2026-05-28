"""Tests for the UI render-target abstractions (FrameBufferRenderer)
and the touch-input bridge.

The framebuffer code mmap's a real device file on the SBC, so the
unit tests here validate the pure-Python parts (RGB565 packing, image
upscale + center, conf parsing, geometry parse) using a temp file as
a stand-in framebuffer. Hardware-bound paths (the actual /dev/fb1
write, the kernel ads7846 evdev open) get exercised on the bench rig
during the install gates.
"""

from __future__ import annotations

import struct
from pathlib import Path

import pytest
from PIL import Image, ImageDraw

from ados.services.ui.renderers import Renderer
from ados.services.ui.renderers.framebuffer import (
    DEFAULT_UPSCALE,
    LOGICAL_HEIGHT,
    LOGICAL_WIDTH,
    FrameBufferRenderer,
    _pack_rgb565,
    _parse_display_conf,
)


# ---------------------------------------------------------------------------
# Renderer protocol — runtime_checkable means a duck-typed instance passes
# ---------------------------------------------------------------------------
class TestRendererProtocol:
    def test_framebuffer_renderer_is_renderer(self, tmp_path: Path):
        # Create a blank 480x320 16bpp framebuffer-like file.
        fb = tmp_path / "fake_fb"
        fb.write_bytes(b"\x00" * (480 * 320 * 2))
        renderer = FrameBufferRenderer(
            fb_path=str(fb),
            fb_name="fb_fake",
            actual_width=480,
            actual_height=320,
            bpp=16,
        )
        try:
            assert isinstance(renderer, Renderer)
            assert renderer.name == "framebuffer"
            assert renderer.width == LOGICAL_WIDTH
            assert renderer.height == LOGICAL_HEIGHT
            assert renderer.actual_width == 480
            assert renderer.actual_height == 320
        finally:
            renderer.cleanup()


# ---------------------------------------------------------------------------
# RGB565 packer
# ---------------------------------------------------------------------------
class TestRgb565Pack:
    def test_pack_solid_red(self):
        img = Image.new("RGB", (2, 1), (255, 0, 0))
        buf = _pack_rgb565(img)
        # Red = 0xF800 little-endian -> 0x00, 0xF8 per pixel
        assert buf == b"\x00\xf8\x00\xf8"

    def test_pack_solid_green(self):
        img = Image.new("RGB", (1, 1), (0, 255, 0))
        buf = _pack_rgb565(img)
        # 6-bit green at top of low byte and low 3 bits of high byte:
        # 0x07E0 little-endian -> 0xE0, 0x07
        assert buf == b"\xe0\x07"

    def test_pack_solid_blue(self):
        img = Image.new("RGB", (1, 1), (0, 0, 255))
        buf = _pack_rgb565(img)
        # 0x001F little-endian -> 0x1F, 0x00
        assert buf == b"\x1f\x00"

    def test_pack_black(self):
        img = Image.new("RGB", (4, 4), (0, 0, 0))
        buf = _pack_rgb565(img)
        assert buf == b"\x00" * 32

    def test_pack_white(self):
        img = Image.new("RGB", (1, 1), (255, 255, 255))
        v_word = struct.unpack("<H", _pack_rgb565(img))[0]
        # All bits set -> 0xFFFF
        assert v_word == 0xFFFF


# ---------------------------------------------------------------------------
# present() blits the upscaled canvas onto the fake framebuffer file
# ---------------------------------------------------------------------------
class TestFrameBufferPresent:
    def _make_fb(self, tmp_path: Path) -> Path:
        # 480x320 16bpp = 307200 bytes. Initialize to all-1 so a black
        # render flips them all to zero (clear test signal).
        fb = tmp_path / "fake_fb"
        fb.write_bytes(b"\xff" * (480 * 320 * 2))
        return fb

    def test_present_black_canvas_writes_zeros(self, tmp_path: Path):
        fb = self._make_fb(tmp_path)
        renderer = FrameBufferRenderer(
            fb_path=str(fb),
            fb_name="fb_fake",
            actual_width=480,
            actual_height=320,
            bpp=16,
        )
        try:
            black = Image.new("1", (LOGICAL_WIDTH, LOGICAL_HEIGHT), 0)
            renderer.present(black)
        finally:
            renderer.cleanup()
        # The whole framebuffer should be zero now.
        data = fb.read_bytes()
        assert all(b == 0 for b in data)

    def test_present_returns_immediately(self, tmp_path: Path):
        """present() must return in <5 ms regardless of mmap.write speed.

        Regression for the LCD freeze cascade: when present() ran
        synchronously, the 10-25 ms SPI write blocked the asyncio
        render loop, which back-pressured the GStreamer appsink and
        triggered not-linked restart loops. The decoupled writer
        thread fix means present() should be near-instant from the
        caller's perspective.
        """
        import time as _time

        fb = self._make_fb(tmp_path)
        renderer = FrameBufferRenderer(
            fb_path=str(fb),
            fb_name="fb_fake",
            actual_width=480,
            actual_height=320,
            bpp=16,
        )
        try:
            img = Image.new("1", (LOGICAL_WIDTH, LOGICAL_HEIGHT), 0)
            t0 = _time.monotonic()
            renderer.present(img)
            elapsed_ms = (_time.monotonic() - t0) * 1000.0
        finally:
            renderer.cleanup()
        # Generous threshold for slow CI; the goal is just to confirm
        # the call no longer waits on the writer thread's mmap.write.
        assert elapsed_ms < 50.0, f"present() took {elapsed_ms:.1f} ms; expected <50"

    def test_present_drops_under_pressure(self, tmp_path: Path):
        """When present() is called faster than the writer can drain,
        the older pending frame is dropped and the writer always sees
        the freshest one. Latest-wins semantics."""
        fb = self._make_fb(tmp_path)
        renderer = FrameBufferRenderer(
            fb_path=str(fb),
            fb_name="fb_fake",
            actual_width=480,
            actual_height=320,
            bpp=16,
        )
        try:
            for _ in range(10):
                renderer.present(Image.new("1", (LOGICAL_WIDTH, LOGICAL_HEIGHT), 0))
        finally:
            renderer.cleanup()
        stats = renderer.stats()
        # All but the last frame should have been dropped.
        assert stats["drops"] >= 1
        assert stats["writes"] >= 1

    def test_present_writes_full_frame_size(self, tmp_path: Path):
        fb = self._make_fb(tmp_path)
        renderer = FrameBufferRenderer(
            fb_path=str(fb),
            fb_name="fb_fake",
            actual_width=480,
            actual_height=320,
            bpp=16,
        )
        try:
            img = Image.new("1", (LOGICAL_WIDTH, LOGICAL_HEIGHT), 1)
            ImageDraw.Draw(img).text((0, 0), "X", fill="white")
            renderer.present(img)
        finally:
            renderer.cleanup()
        # File size must be unchanged (no extra bytes written, no truncation).
        assert fb.stat().st_size == 480 * 320 * 2

    def test_upscale_factor_default(self, tmp_path: Path):
        fb = self._make_fb(tmp_path)
        renderer = FrameBufferRenderer(
            fb_path=str(fb),
            fb_name="fb_fake",
            actual_width=480,
            actual_height=320,
            bpp=16,
        )
        try:
            assert renderer.upscale == DEFAULT_UPSCALE
        finally:
            renderer.cleanup()


# ---------------------------------------------------------------------------
# /etc/ados/display.conf parser
# ---------------------------------------------------------------------------
class TestDisplayConfParse:
    def test_missing_file_returns_empty(self, monkeypatch, tmp_path: Path):
        bogus = tmp_path / "nope.conf"
        monkeypatch.setattr(
            "ados.services.ui.renderers.framebuffer.DISPLAY_CONF_PATH", bogus
        )
        assert _parse_display_conf() == {}

    def test_simple_keys(self, monkeypatch, tmp_path: Path):
        conf = tmp_path / "display.conf"
        conf.write_text(
            "display_id=waveshare35a\n"
            "controller=ILI9486\n"
            "has_touch=true\n"
            "# this is a comment\n"
            "\n"
            "framebuffer_path=/dev/fb1\n"
        )
        monkeypatch.setattr(
            "ados.services.ui.renderers.framebuffer.DISPLAY_CONF_PATH", conf
        )
        out = _parse_display_conf()
        assert out["display_id"] == "waveshare35a"
        assert out["controller"] == "ILI9486"
        assert out["has_touch"] == "true"
        assert out["framebuffer_path"] == "/dev/fb1"
        assert "# this is a comment" not in out


# ---------------------------------------------------------------------------
# probe() returns None when the framebuffer file is absent
# ---------------------------------------------------------------------------
class TestFrameBufferProbe:
    def test_probe_absent_returns_none(self, monkeypatch, tmp_path: Path):
        # No conf file, default fb path /dev/fb1 won't exist on the dev
        # box. Probe should return None.
        bogus = tmp_path / "nope.conf"
        monkeypatch.setattr(
            "ados.services.ui.renderers.framebuffer.DISPLAY_CONF_PATH", bogus
        )
        # Skip on the off chance someone has a real /dev/fb1 on their
        # dev machine — we don't want to bind it from a test.
        if Path("/dev/fb1").exists():
            pytest.skip("test host has a real /dev/fb1; skipping probe-absent test")
        assert FrameBufferRenderer.probe() is None


class TestFrameBufferProbeByName:
    """probe() selects the framebuffer by driver NAME across all fb indices.

    Drives probe() against a mocked /sys/class/graphics + /dev tree so the
    name-matching + acceptance logic is exercised without real hardware.
    """

    def _mock_fb_tree(self, monkeypatch, tmp_path: Path, fbs: dict[str, str]):
        """fbs maps fb index name -> driver name. Returns the bound fb name
        probe() picks, or None.

        Mocks: SYS_FB_GLOB.iterdir() to yield the fb dirs, the per-fb name +
        geometry readers, the conf parse, and Path.exists for /dev/fbN.
        """
        import ados.services.ui.renderers.framebuffer as fbmod

        sysg = tmp_path / "sys_graphics"
        sysg.mkdir()
        for name, driver in fbs.items():
            d = sysg / name
            d.mkdir()
            (d / "name").write_text(driver + "\n")
            (d / "virtual_size").write_text("480,320\n")
            (d / "bits_per_pixel").write_text("16\n")

        monkeypatch.setattr(fbmod, "SYS_FB_GLOB", sysg)
        # No conf -> empty expected name path; force the no-expected branch.
        monkeypatch.setattr(fbmod, "_parse_display_conf", lambda: {})
        monkeypatch.setattr(fbmod, "read_rotation", lambda: 0)
        # /dev/fbN exists for each mocked fb; nothing else.
        real_exists = Path.exists
        dev_fbs = {f"/dev/{n}" for n in fbs}

        def _fake_exists(self):  # noqa: ANN001
            s = str(self)
            if s.startswith("/dev/fb"):
                return s in dev_fbs
            return real_exists(self)

        monkeypatch.setattr(Path, "exists", _fake_exists)
        # Don't actually open/mmap a device — stop after selection by making
        # the constructor raise; probe() returns the renderer, so instead we
        # intercept __init__ to capture the chosen fb_name and short-circuit.
        captured: dict[str, str] = {}

        def _fake_init(self, *a, **kw):  # noqa: ANN001
            captured["fb_name"] = kw.get("fb_name", "")
            raise _StopProbe()

        class _StopProbe(Exception):
            pass

        monkeypatch.setattr(FrameBufferRenderer, "__init__", _fake_init)
        try:
            FrameBufferRenderer.probe()
        except _StopProbe:
            return captured.get("fb_name")
        return None

    def test_picks_spi_lcd_over_hdmi_no_expected(self, monkeypatch, tmp_path):
        # fb0 = HDMI/DRM primary, fb1 = SPI LCD. No expected name configured
        # -> must accept ONLY the known SPI-LCD driver, never the HDMI fb.
        chosen = self._mock_fb_tree(
            monkeypatch,
            tmp_path,
            {"fb0": "rockchip-drm", "fb1": "fb_ili9486"},
        )
        assert chosen == "fb1"

    def test_rejects_non_spi_only_fb_no_expected(self, monkeypatch, tmp_path):
        # Only an HDMI framebuffer present, no expected name -> probe must
        # NOT bind it (returns None), so the UI never paints onto HDMI.
        chosen = self._mock_fb_tree(
            monkeypatch, tmp_path, {"fb0": "BCM2708 FB"}
        )
        assert chosen is None

    def test_binds_spi_lcd_on_fb0_headless(self, monkeypatch, tmp_path):
        # Headless rig: SPI LCD lands on fb0. Bound by name, not index.
        chosen = self._mock_fb_tree(
            monkeypatch, tmp_path, {"fb0": "fb_ili9486"}
        )
        assert chosen == "fb0"
