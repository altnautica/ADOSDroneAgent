"""Tests for FrameBufferRenderer rotation handling.

The Waveshare 3.5" RPi LCD (A) is mounted physically rotated on most
ADOS dev rigs. The kernel-managed framebuffer is a flat byte array;
the only way to compensate for a physical rotation is to rotate the
PIL canvas before packing it into the framebuffer mmap.

These tests use a real mmap of a tmp file as the framebuffer backing
store so the production code path runs unchanged. The renderer is
constructed with the actual width / height matching the canvas size
so each test can verify that the correct corner of the canvas lands
at the correct corner of the byte array after rotation.

The ``_pack_rgb565`` helper packs every two bytes as a little-endian
RGB565 word, so a (255, 0, 0) red pixel encodes as ``0xF800`` which
serializes as bytes ``00 F8`` (low byte first).

We use a small square canvas (8x8) so PIL's ``rotate(angle,
expand=False)`` keeps the marker pixel inside the frame at every
rotation. A non-square canvas at 90/270 with expand=False would
crop the long axis; that is acceptable in production because the
real panel reports its rotated geometry through the kernel, but it
would make these unit tests brittle.
"""

from __future__ import annotations

from pathlib import Path

import pytest
from PIL import Image

from ados.services.ui.renderers.framebuffer import FrameBufferRenderer


def _make_renderer(
    tmp_path: Path,
    width: int,
    height: int,
    rotation: int,
    bpp: int = 16,
) -> tuple[FrameBufferRenderer, Path]:
    """Construct a renderer backed by a real mmap of a tmp file.

    Returns the renderer and the framebuffer path so tests can read
    the bytes back after present().
    """
    fb_bytes = width * height * (bpp // 8)
    fb_path = tmp_path / "fb_fake"
    with open(fb_path, "wb") as fh:
        fh.write(b"\x00" * fb_bytes)

    renderer = FrameBufferRenderer(
        fb_path=str(fb_path),
        fb_name="fb_fake",
        actual_width=width,
        actual_height=height,
        bpp=bpp,
        rotation=rotation,
    )
    return renderer, fb_path


def _read_rgb565_pixel(buf: bytes, x: int, y: int, width: int) -> tuple[int, int, int]:
    """Decode the RGB565 pixel at (x, y) from a packed framebuffer buffer."""
    offset = (y * width + x) * 2
    lo = buf[offset]
    hi = buf[offset + 1]
    word = (hi << 8) | lo
    r = (word >> 11) & 0x1F
    g = (word >> 5) & 0x3F
    b = word & 0x1F
    return (r << 3) | (r >> 2), (g << 2) | (g >> 4), (b << 3) | (b >> 2)


def _solid_with_marker(
    width: int, height: int, marker_xy: tuple[int, int],
) -> Image.Image:
    """Black canvas with one red pixel at marker_xy."""
    img = Image.new("RGB", (width, height), (0, 0, 0))
    img.putpixel(marker_xy, (255, 0, 0))
    return img


def _find_red_pixel(
    buf: bytes, width: int, height: int,
) -> tuple[int, int] | None:
    """Return (x, y) of the first red pixel in the framebuffer buffer."""
    for y in range(height):
        for x in range(width):
            r, _, _ = _read_rgb565_pixel(buf, x, y, width)
            if r > 200:
                return x, y
    return None


# ---------------------------------------------------------------------------
# Square 8x8 canvas — every rotation keeps the marker inside the frame.
# ---------------------------------------------------------------------------


class TestRotationZero:
    def test_marker_at_origin_lands_at_origin(self, tmp_path):
        renderer, fb_path = _make_renderer(tmp_path, 8, 8, rotation=0)
        try:
            img = _solid_with_marker(8, 8, (0, 0))
            renderer.present(img)
            buf = fb_path.read_bytes()
            assert _find_red_pixel(buf, 8, 8) == (0, 0)
        finally:
            renderer.cleanup()


class TestRotation90:
    def test_marker_at_origin_lands_at_top_right(self, tmp_path):
        # The renderer applies ``image.rotate(-self.rotation)``. For
        # rotation=90 that is ``rotate(-90)``, which PIL implements as
        # a 90-degree visual rotation that maps source (0, 0) -> dest
        # (w-1, 0): the top-right of the rotated frame.
        renderer, fb_path = _make_renderer(tmp_path, 8, 8, rotation=90)
        try:
            img = _solid_with_marker(8, 8, (0, 0))
            renderer.present(img)
            buf = fb_path.read_bytes()
            assert _find_red_pixel(buf, 8, 8) == (7, 0)
        finally:
            renderer.cleanup()

    def test_marker_at_bottom_left_lands_at_origin(self, tmp_path):
        # Marker at (0, 7) on the source ends up at (0, 0) on the dest
        # under ``rotate(-90)``.
        renderer, fb_path = _make_renderer(tmp_path, 8, 8, rotation=90)
        try:
            img = _solid_with_marker(8, 8, (0, 7))
            renderer.present(img)
            buf = fb_path.read_bytes()
            assert _find_red_pixel(buf, 8, 8) == (0, 0)
        finally:
            renderer.cleanup()


class TestRotation180:
    def test_marker_at_origin_lands_at_far_corner(self, tmp_path):
        renderer, fb_path = _make_renderer(tmp_path, 8, 8, rotation=180)
        try:
            img = _solid_with_marker(8, 8, (0, 0))
            renderer.present(img)
            buf = fb_path.read_bytes()
            # 180 deg flips both axes: (0,0) -> (w-1, h-1)
            assert _find_red_pixel(buf, 8, 8) == (7, 7)
        finally:
            renderer.cleanup()


class TestRotation270:
    def test_marker_at_origin_lands_at_bottom_left(self, tmp_path):
        # ``rotate(-270)`` is the inverse of ``rotate(-90)`` and maps
        # source (0, 0) -> dest (0, w-1): bottom-left of the rotated
        # frame.
        renderer, fb_path = _make_renderer(tmp_path, 8, 8, rotation=270)
        try:
            img = _solid_with_marker(8, 8, (0, 0))
            renderer.present(img)
            buf = fb_path.read_bytes()
            assert _find_red_pixel(buf, 8, 8) == (0, 7)
        finally:
            renderer.cleanup()


# ---------------------------------------------------------------------------
# Normalization + attribute storage
# ---------------------------------------------------------------------------


class TestRotationNormalization:
    def test_invalid_rotation_falls_back_to_zero(self, tmp_path):
        renderer, fb_path = _make_renderer(tmp_path, 8, 8, rotation=45)
        try:
            assert renderer.rotation == 0
            img = _solid_with_marker(8, 8, (0, 0))
            renderer.present(img)
            buf = fb_path.read_bytes()
            assert _find_red_pixel(buf, 8, 8) == (0, 0)
        finally:
            renderer.cleanup()

    def test_rotation_attribute_stored(self, tmp_path):
        for rot in (0, 90, 180, 270):
            renderer, _ = _make_renderer(tmp_path, 8, 8, rotation=rot)
            try:
                assert renderer.rotation == rot
            finally:
                renderer.cleanup()

    def test_rotation_negative_normalizes_to_legal(self, tmp_path):
        # -90 % 360 = 270 — legal value, should be stored as 270.
        renderer, _ = _make_renderer(tmp_path, 8, 8, rotation=-90)
        try:
            assert renderer.rotation == 270
        finally:
            renderer.cleanup()


# ---------------------------------------------------------------------------
# Solid-fill sanity at every rotation: bytes stay framebuffer-sized.
# ---------------------------------------------------------------------------


class TestRotationFramebufferSize:
    @pytest.mark.parametrize("rotation", [0, 90, 180, 270])
    def test_solid_white_fills_full_frame(self, tmp_path, rotation):
        renderer, fb_path = _make_renderer(
            tmp_path, 8, 8, rotation=rotation,
        )
        try:
            img = Image.new("RGB", (8, 8), (255, 255, 255))
            renderer.present(img)
            buf = fb_path.read_bytes()
            assert len(buf) == 8 * 8 * 2
            # Every pixel should be near-white regardless of rotation
            # because the input is a solid fill.
            for y in range(8):
                for x in range(8):
                    r, g, b = _read_rgb565_pixel(buf, x, y, 8)
                    assert r > 200 and g > 200 and b > 200
        finally:
            renderer.cleanup()
