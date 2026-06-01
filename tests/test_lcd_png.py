"""Tests for the pure-Python LCD PNG encoder + framebuffer decode.

The snapshot endpoint must serve a valid PNG without Pillow. These tests
encode known RGB888 bytes and decode them back with ``zlib`` + ``struct``
(the same primitives the encoder uses) to confirm the byte shape, and
exercise the framebuffer reader against a fake ``/dev/fbN`` + sysfs layout
for the three pixel depths the SPI panel exposes.
"""

from __future__ import annotations

import struct
import zlib
from pathlib import Path

import pytest

from ados.api.routes import _lcd_png


def _decode_png(data: bytes) -> tuple[int, int, bytes]:
    """Minimal truecolour-PNG decoder: returns (width, height, rgb888).

    Validates the signature, walks the chunks, inflates the IDAT, and
    strips the per-scanline filter byte (which the encoder always sets to
    0). Asserts the basics so a malformed encode fails loudly.
    """
    assert data[:8] == b"\x89PNG\r\n\x1a\n"
    pos = 8
    width = height = 0
    idat = bytearray()
    while pos < len(data):
        length = struct.unpack(">I", data[pos : pos + 4])[0]
        tag = data[pos + 4 : pos + 8]
        body = data[pos + 8 : pos + 8 + length]
        crc = struct.unpack(">I", data[pos + 8 + length : pos + 12 + length])[0]
        assert crc == zlib.crc32(tag + body) & 0xFFFFFFFF
        if tag == b"IHDR":
            width, height, depth, ctype = struct.unpack(">IIBB", body[:10])
            assert depth == 8
            assert ctype == 2  # truecolour
        elif tag == b"IDAT":
            idat += body
        pos += 12 + length

    raw = zlib.decompress(bytes(idat))
    stride = width * 3
    out = bytearray()
    for y in range(height):
        start = y * (stride + 1)
        assert raw[start] == 0  # filter type None
        out += raw[start + 1 : start + 1 + stride]
    return width, height, bytes(out)


def test_encode_round_trips_rgb888() -> None:
    rgb = bytes(
        [
            255, 0, 0,  # red
            0, 255, 0,  # green
            0, 0, 255,  # blue
            255, 255, 255,  # white
        ]
    )
    png = _lcd_png.encode_png_rgb888(rgb, 2, 2)
    w, h, decoded = _decode_png(png)
    assert (w, h) == (2, 2)
    assert decoded == rgb


def test_encode_rejects_size_mismatch() -> None:
    with pytest.raises(ValueError):
        _lcd_png.encode_png_rgb888(b"\x00\x01\x02", 2, 2)


def _fake_fb(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, *, bpp: int, payload: bytes
) -> str:
    """Stage a fake ``/dev/fbX`` device file + sysfs geometry, returning the
    device path. The sysfs reads in ``_lcd_png`` are redirected to tmp_path."""
    dev = tmp_path / "fb9"
    dev.write_bytes(payload)
    sysfs = tmp_path / "sys" / "class" / "graphics" / "fb9"
    sysfs.mkdir(parents=True)
    (sysfs / "virtual_size").write_text("2,1")
    (sysfs / "bits_per_pixel").write_text(str(bpp))

    real_path_cls = _lcd_png.Path

    def fake_path(arg: str) -> Path:
        if str(arg).startswith("/sys/class/graphics/"):
            name = real_path_cls(arg).name
            return tmp_path / "sys" / "class" / "graphics" / name
        return real_path_cls(arg)

    monkeypatch.setattr(_lcd_png, "Path", fake_path)
    return str(dev)


def test_render_framebuffer_png_rgb24(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Two RGB24 pixels: red, green.
    payload = bytes([255, 0, 0, 0, 255, 0])
    dev = _fake_fb(tmp_path, monkeypatch, bpp=24, payload=payload)
    png = _lcd_png.render_framebuffer_png(dev)
    assert png is not None
    w, h, rgb = _decode_png(png)
    assert (w, h) == (2, 1)
    assert rgb == payload


def test_render_framebuffer_png_rgb565(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Two RGB565 LE pixels: pure red (0xF800) and pure blue (0x001F).
    payload = struct.pack("<HH", 0xF800, 0x001F)
    dev = _fake_fb(tmp_path, monkeypatch, bpp=16, payload=payload)
    png = _lcd_png.render_framebuffer_png(dev)
    assert png is not None
    w, h, rgb = _decode_png(png)
    assert (w, h) == (2, 1)
    # 5-bit red 0x1F → 0xFF; 5-bit blue 0x1F → 0xFF.
    assert rgb[0:3] == bytes([255, 0, 0])
    assert rgb[3:6] == bytes([0, 0, 255])


def test_render_framebuffer_png_xrgb32(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Two xRGB32 pixels, byte order B,G,R,X: red then green.
    payload = bytes([0, 0, 255, 0, 0, 255, 0, 0])
    dev = _fake_fb(tmp_path, monkeypatch, bpp=32, payload=payload)
    png = _lcd_png.render_framebuffer_png(dev)
    assert png is not None
    w, h, rgb = _decode_png(png)
    assert (w, h) == (2, 1)
    assert rgb == bytes([255, 0, 0, 0, 255, 0])


def test_render_framebuffer_png_rejects_short_read(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Geometry says 2x1x24 = 6 bytes, but the device only has 3.
    dev = _fake_fb(tmp_path, monkeypatch, bpp=24, payload=b"\x00\x01\x02")
    assert _lcd_png.render_framebuffer_png(dev) is None


def test_render_framebuffer_png_rejects_unsupported_depth(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    dev = _fake_fb(tmp_path, monkeypatch, bpp=8, payload=b"\x00\x01")
    assert _lcd_png.render_framebuffer_png(dev) is None
