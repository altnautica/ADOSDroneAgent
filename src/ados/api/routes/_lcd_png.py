"""Pure-Python PNG encode + framebuffer decode for the LCD snapshot.

The native display writer (``ados-display``) drops a PNG of the live panel
at ``/run/ados/lcd-snapshot.png`` after every render, and the snapshot
endpoint serves that file directly. When the file is missing or stale
(the writer has not rendered yet, or the legacy fallback UI is running)
the endpoint falls back to reading the kernel framebuffer itself. This
module is that fallback: it reads ``/dev/fbN``, unpacks the panel's pixel
format to RGB888, and encodes a PNG with the standard library only —
``zlib`` + ``struct`` — so the API process never depends on Pillow.

The encoder writes a minimal truecolour (RGB, 8-bit) PNG: signature, an
``IHDR``, one zlib-compressed ``IDAT`` of filter-0 scanlines, and an
``IEND``. That is the same byte shape the Rust writer's ``png`` crate
emits, so the GCS ``<img>`` preview consumes either source identically.
"""

from __future__ import annotations

import struct
import zlib
from pathlib import Path

# Pixel depths the fbtft / DRM SPI panel can expose. Other depths are
# rejected because the byte ordering would have to be guessed.
_SUPPORTED_BPP: frozenset[int] = frozenset({16, 24, 32})


def encode_png_rgb888(rgb888: bytes, width: int, height: int) -> bytes:
    """Encode tightly-packed RGB888 bytes into a truecolour PNG.

    ``rgb888`` must be exactly ``width * height * 3`` bytes, row-major,
    with no per-row padding. Raises ``ValueError`` on a size mismatch so a
    truncated framebuffer read never produces a malformed image.
    """
    expected = width * height * 3
    if len(rgb888) != expected:
        raise ValueError(
            f"rgb888 is {len(rgb888)} bytes, expected {expected} "
            f"for {width}x{height}"
        )

    def _chunk(tag: bytes, data: bytes) -> bytes:
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
        )

    # IHDR: width, height, bit depth 8, colour type 2 (truecolour),
    # default compression / filter / interlace.
    ihdr = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)

    # Each scanline is prefixed with filter type 0 (None).
    stride = width * 3
    raw = bytearray()
    for y in range(height):
        raw.append(0)
        start = y * stride
        raw += rgb888[start : start + stride]
    idat = zlib.compress(bytes(raw), 6)

    return (
        b"\x89PNG\r\n\x1a\n"
        + _chunk(b"IHDR", ihdr)
        + _chunk(b"IDAT", idat)
        + _chunk(b"IEND", b"")
    )


def _resolve_fb_geometry(fb_name: str) -> tuple[int, int, int] | None:
    """Read ``(width, height, bpp)`` for ``/dev/<fb_name>`` from sysfs.

    Returns ``None`` when the geometry cannot be read or the depth is not
    one of the three the SPI panel exposes.
    """
    sysfs = Path(f"/sys/class/graphics/{fb_name}")
    try:
        virtual = (sysfs / "virtual_size").read_text().strip()
        bpp_str = (sysfs / "bits_per_pixel").read_text().strip()
        xres_s, yres_s = virtual.split(",", 1)
        xres = int(xres_s)
        yres = int(yres_s)
        bpp = int(bpp_str)
    except (OSError, ValueError):
        return None
    if xres <= 0 or yres <= 0 or bpp not in _SUPPORTED_BPP:
        return None
    return xres, yres, bpp


def _unpack_to_rgb888(buf: bytes, count: int, bpp: int) -> bytes:
    """Convert a framebuffer byte run to RGB888 for ``count`` pixels.

    Supports the three SPI-panel formats: RGB565 little-endian (16 bpp),
    RGB24 passthrough (24 bpp), and xRGB32 where the bytes are B,G,R,X
    (32 bpp). The caller guarantees ``bpp`` is one of these.
    """
    if bpp == 24:
        return buf[: count * 3]

    out = bytearray(count * 3)
    if bpp == 16:
        for i in range(count):
            lo = buf[2 * i]
            hi = buf[2 * i + 1]
            pix = lo | (hi << 8)
            r = (pix >> 11) & 0x1F
            g = (pix >> 5) & 0x3F
            b = pix & 0x1F
            out[3 * i + 0] = (r << 3) | (r >> 2)
            out[3 * i + 1] = (g << 2) | (g >> 4)
            out[3 * i + 2] = (b << 3) | (b >> 2)
    else:  # 32 bpp xRGB; bytes are B,G,R,X
        for i in range(count):
            out[3 * i + 0] = buf[4 * i + 2]  # R
            out[3 * i + 1] = buf[4 * i + 1]  # G
            out[3 * i + 2] = buf[4 * i + 0]  # B
    return bytes(out)


def render_framebuffer_png(fb_path: str) -> bytes | None:
    """Read ``fb_path``, unpack to RGB888, and encode a full-panel PNG.

    Returns ``None`` when the framebuffer is missing, the geometry cannot
    be read, the depth is unsupported, or the read is short. No
    downsampling: the panel is small (480x320 at most) and the GCS scales
    the image client-side, so a full-resolution PNG keeps this path free
    of any image-resize dependency.
    """
    fb_name = Path(fb_path).name
    geom = _resolve_fb_geometry(fb_name)
    if geom is None:
        return None
    xres, yres, bpp = geom

    count = xres * yres
    want = count * (bpp // 8)
    try:
        with open(fb_path, "rb") as fh:
            buf = fh.read(want)
    except OSError:
        return None
    if len(buf) < want:
        return None

    rgb888 = _unpack_to_rgb888(buf, count, bpp)
    return encode_png_rgb888(rgb888, xres, yres)


__all__ = ["encode_png_rgb888", "render_framebuffer_png"]
