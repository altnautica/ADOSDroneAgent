"""Framebuffer renderer for SPI LCDs (ILI9486 / fb_ili9486).

Targets the kernel-managed framebuffer device that ``fbtft`` exposes
once the LCD device-tree overlay binds. Reads the actual hardware
resolution and bit depth from ``/sys/class/graphics/<fb>/var``, then
mmap's the framebuffer for direct pixel writes.

Phase-1 scaling strategy: the screen modules are tuned for the 128x64
OLED. Rendering them at native LCD size would put a postage-stamp
display in the corner. Instead, ``present()`` upscales the 128x64
canvas 4x with NEAREST and centers it on a black background of the
actual hardware resolution. Result is a crisp pixel-art status
display on the 480x320 LCD.

Phase-2 work (separate plan) will add a native-resolution render path
with a larger font set and 480x320-tuned coordinates, plus a Studio
view that uses the extra pixels for richer dashboards.
"""

from __future__ import annotations

import mmap
import os
import struct
import threading
from pathlib import Path
from typing import TYPE_CHECKING

from PIL import Image

from ados.core.logging import get_logger
from ados.core.paths import DISPLAY_CONF_PATH
from ados.services.ui.display_conf import read_rotation

if TYPE_CHECKING:  # pragma: no cover
    from PIL.Image import Image as PILImage


log = get_logger("ui.framebuffer")


# Logical canvas the screen modules paint onto. Matches the 128x64
# dimensions baked into the screens. ``present()`` upscales from here.
LOGICAL_WIDTH = 128
LOGICAL_HEIGHT = 64

# Default upscale factor. 4x of 128x64 is 512x256, which fits centered
# on a 480x320 panel with horizontal letterbox cropped to 480.
DEFAULT_UPSCALE = 4

# /sys path templates. fb_ili9486 typically binds as /dev/fb1 because
# /dev/fb0 is the primary HDMI / DRM framebuffer (when present).
SYS_FB_GLOB = Path("/sys/class/graphics")


def _read_fb_geometry(fb_name: str) -> tuple[int, int, int]:
    """Return ``(xres, yres, bits_per_pixel)`` for a framebuffer.

    Reads the standard sysfs framebuffer attributes. Newer kernels
    expose ``virtual_size`` (``WIDTH,HEIGHT``) and ``bits_per_pixel``
    as individual files. Older kernels (and a few fbtft builds) only
    expose the legacy ``var`` blob — try that as a fallback.
    """
    fb_dir = SYS_FB_GLOB / fb_name
    vsize_path = fb_dir / "virtual_size"
    bpp_path = fb_dir / "bits_per_pixel"
    if vsize_path.exists() and bpp_path.exists():
        vsize = vsize_path.read_text().strip()
        if "," in vsize:
            w_str, h_str = vsize.split(",", 1)
        else:
            parts = vsize.split()
            if len(parts) < 2:
                raise OSError(f"unexpected virtual_size format: {vsize!r}")
            w_str, h_str = parts[0], parts[1]
        xres = int(w_str.strip())
        yres = int(h_str.strip())
        bpp = int(bpp_path.read_text().strip())
        return xres, yres, bpp
    # Legacy fallback: parse the show_var() blob.
    var_path = fb_dir / "var"
    text = var_path.read_text().strip()
    parts = text.split()
    if len(parts) < 7:
        raise OSError(f"unexpected /sys/class/graphics/{fb_name}/var format")
    xres = int(parts[0])
    yres = int(parts[1])
    bpp = int(parts[6])
    return xres, yres, bpp


def _read_fb_name(fb_name: str) -> str:
    """Return the driver-reported name from /sys/class/graphics/<fb>/name."""
    path = SYS_FB_GLOB / fb_name / "name"
    try:
        return path.read_text().strip()
    except OSError:
        return ""


def _parse_display_conf() -> dict[str, str]:
    """Parse the simple key=value /etc/ados/display.conf."""
    out: dict[str, str] = {}
    if not DISPLAY_CONF_PATH.exists():
        return out
    try:
        for raw in DISPLAY_CONF_PATH.read_text().splitlines():
            line = raw.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            k, _, v = line.partition("=")
            out[k.strip()] = v.strip()
    except OSError as exc:
        log.warning("display_conf_read_failed", error=str(exc))
    return out


def _pack_rgb565(image: PILImage) -> bytes:
    """Convert an RGB PIL image to packed RGB565 little-endian bytes."""
    rgb = image.convert("RGB")
    pixels = rgb.tobytes()
    out = bytearray(len(pixels) // 3 * 2)
    # Inline pack: avoids a per-pixel Python loop.
    for i in range(0, len(pixels), 3):
        r = pixels[i]
        g = pixels[i + 1]
        b = pixels[i + 2]
        v = ((r & 0xF8) << 8) | ((g & 0xFC) << 3) | (b >> 3)
        struct.pack_into("<H", out, (i // 3) * 2, v)
    return bytes(out)


class FrameBufferRenderer:
    """Render to a kernel framebuffer device (ILI9486 via fbtft).

    Construct via :meth:`probe` to honor the auto-detect path that
    consults ``/etc/ados/display.conf`` and confirms the bound driver
    name matches expectations. Direct construction is supported for
    tests that pass a fake framebuffer path.
    """

    name = "framebuffer"

    def __init__(
        self,
        fb_path: str = "/dev/fb1",
        fb_name: str = "fb1",
        actual_width: int = 480,
        actual_height: int = 320,
        bpp: int = 16,
        upscale: int = DEFAULT_UPSCALE,
        rotation: int = 0,
    ) -> None:
        self._fb_path = fb_path
        self._fb_name = fb_name
        self.actual_width = actual_width
        self.actual_height = actual_height
        self.bpp = bpp
        self.upscale = upscale
        # Logical canvas size the service paints onto.
        self.width = LOGICAL_WIDTH
        self.height = LOGICAL_HEIGHT
        # Display rotation in degrees (0 / 90 / 180 / 270). Applied to
        # the canvas in present() before mmap write so a panel mounted
        # rotated 90 degrees physically still renders the UI right-way-up.
        self.rotation: int = int(rotation) % 360 if int(rotation) % 360 in (
            0, 90, 180, 270,
        ) else 0
        self._fd: int = -1
        self._mmap: mmap.mmap | None = None
        self._frame_bytes = actual_width * actual_height * (bpp // 8)
        # Decoupled SPI writer. present() stashes the latest image
        # into _pending_image (latest-wins) and signals the writer
        # thread; the actual PIL composition + RGB565 pack + mmap.write
        # runs on the writer's daemon thread. Removes the 10-25 ms SPI
        # blocking call from the asyncio render loop's critical path,
        # which was the root cascade behind the LCD freeze: synchronous
        # mmap.write blocks the loop -> appsink pulls slow -> upstream
        # queues fill -> rtspsrc stalls -> not-linked bus error -> tap
        # restart. Latest-wins semantics: if SPI is busy when a new
        # frame arrives, the older pending frame is dropped.
        self._pending_image: PILImage | None = None
        self._pending_lock = threading.Lock()
        self._pending_event = threading.Event()
        self._writer_stop = threading.Event()
        self._writer_drops: int = 0
        self._writer_writes: int = 0
        self._writer_skipped_duplicates: int = 0
        # Cached hash of the most recently packed-and-written image.
        # Subsequent frames whose raw bytes hash to the same value skip
        # the RGB565 pack + mmap.write entirely — a status carousel
        # spends most of its time showing the same pixels and packing
        # 480x320 in pure-Python costs ~100 ms per call on a Pi 4B.
        self._last_written_hash: int | None = None
        # Last-tick timing observability. Wall-clock duration of the
        # most recent mmap.write() call; surfaced via stats() so an
        # SPI-throughput regression is visible in journalctl + the
        # GCS / LCD diagnostics page.
        self._last_write_ms: float | None = None
        self._writer_thread: threading.Thread | None = None
        self._open()
        # Spawn the writer only after _open() succeeds — _open() raises
        # on missing framebuffer and we don't want a thread holding a
        # half-initialized renderer.
        self._writer_thread = threading.Thread(
            target=self._writer_loop,
            name="ados-fb-writer",
            daemon=True,
        )
        self._writer_thread.start()

    @classmethod
    def probe(cls) -> FrameBufferRenderer | None:
        """Return a renderer if a matching framebuffer is bound, else None.

        Honors ``/etc/ados/display.conf`` written by the LCD-overlay
        installer for the configured path. Also scans
        ``/sys/class/graphics/*`` for any framebuffer whose driver name
        matches ``framebuffer_name_expected`` (default ``fb_ili9486``).
        On a headless rig the kernel can assign the SPI LCD to
        ``/dev/fb0`` because the Rockchip DRM driver doesn't claim
        ``fb0`` when no HDMI monitor is attached, so we can't hardcode
        ``/dev/fb1``.
        """
        conf = _parse_display_conf()
        configured_path = conf.get("framebuffer_path", "/dev/fb1")
        expected = (conf.get("framebuffer_name_expected") or "fb_ili9486").strip()
        # Honor the rotation key the LCD-overlay installer wrote (and
        # the settings page may have toggled). 0 / 90 / 180 / 270 are
        # the only legal values; the display_conf helper returns 0 on
        # any malformed input.
        rotation = read_rotation()

        # Build candidate list: configured path first, then every
        # /dev/fb* that has a /sys/class/graphics/<name>/var entry.
        candidates: list[str] = []
        if Path(configured_path).exists():
            candidates.append(configured_path)
        if SYS_FB_GLOB.exists():
            for entry in sorted(SYS_FB_GLOB.iterdir()):
                if not entry.name.startswith("fb"):
                    continue
                dev_path = f"/dev/{entry.name}"
                if dev_path not in candidates and Path(dev_path).exists():
                    candidates.append(dev_path)

        if not candidates:
            log.debug("framebuffer_absent", configured=configured_path)
            return None

        for candidate in candidates:
            fb_name = Path(candidate).name
            try:
                xres, yres, bpp = _read_fb_geometry(fb_name)
            except OSError as exc:
                log.debug(
                    "framebuffer_geometry_unreadable",
                    fb=fb_name,
                    error=str(exc),
                )
                continue
            driver_name = _read_fb_name(fb_name)
            if expected and driver_name and expected not in driver_name:
                log.debug(
                    "framebuffer_driver_skip",
                    fb=fb_name,
                    driver=driver_name,
                    expected=expected,
                )
                continue
            if bpp not in (16, 24, 32):
                log.warning(
                    "framebuffer_bpp_unsupported", fb=fb_name, bpp=bpp,
                )
                continue
            log.info(
                "framebuffer_probed",
                path=candidate,
                name=driver_name,
                width=xres,
                height=yres,
                bpp=bpp,
                rotation=rotation,
            )
            return cls(
                fb_path=candidate,
                fb_name=fb_name,
                actual_width=xres,
                actual_height=yres,
                bpp=bpp,
                rotation=rotation,
            )

        log.debug(
            "framebuffer_no_match",
            expected=expected,
            checked=candidates,
        )
        return None

    def _open(self) -> None:
        try:
            self._fd = os.open(self._fb_path, os.O_RDWR)
        except OSError as exc:
            log.warning("framebuffer_open_failed", path=self._fb_path, error=str(exc))
            raise
        try:
            self._mmap = mmap.mmap(self._fd, self._frame_bytes)
        except (OSError, ValueError) as exc:
            log.warning("framebuffer_mmap_failed", path=self._fb_path, error=str(exc))
            os.close(self._fd)
            self._fd = -1
            raise

    def present(self, image: PILImage) -> None:
        """Stash the image for the SPI writer thread; return immediately.

        Used to do the PIL composition + RGB565 pack + mmap.write
        synchronously, blocking the caller for 10-25 ms. That stalled
        the OLED service's asyncio loop, which back-pressured the
        GStreamer appsink, which back-pressured the queues + decoder,
        which eventually starved rtspsrc and tripped the not-linked
        restart cascade.

        Now: the calling thread only stashes the image into a single-
        slot lock-protected holder and signals the writer. The writer
        thread (``ados-fb-writer``) does the composition + pack + mmap
        write off-thread. If a new frame arrives while the writer is
        still busy with the previous one, latest-wins: the older
        pending frame is dropped.
        """
        if self._mmap is None:
            return
        with self._pending_lock:
            if self._pending_image is not None:
                # The previous frame never made it to SPI — overwrite
                # so the writer always handles the freshest frame.
                self._writer_drops += 1
            self._pending_image = image
        self._pending_event.set()

    def _compose_and_pack(self, image: PILImage) -> bytes | None:
        """Build the RGB565 / RGB888 / xRGB byte buffer for the panel.

        Pulled out of the original ``present`` body unchanged: same
        size-matching, same upscale path for legacy screens, same
        rotation, same bpp pack. Returns ``None`` if the produced buffer
        size mismatches the framebuffer geometry (caller logs + drops).
        """
        if image.size == (self.actual_width, self.actual_height):
            # Native dashboard render. No scaling, just convert mode if
            # needed and treat the image itself as the canvas.
            canvas = image if image.mode == "RGB" else image.convert("RGB")
        else:
            # Legacy upscale path for the 128x64 OLED carousel screens.
            if image.mode != "RGB":
                scaled_src = image.convert("RGB")
            else:
                scaled_src = image
            scaled_w = self.width * self.upscale
            scaled_h = self.height * self.upscale
            scaled = scaled_src.resize(
                (scaled_w, scaled_h), resample=Image.NEAREST,
            )

            canvas = Image.new(
                "RGB", (self.actual_width, self.actual_height), (0, 0, 0),
            )
            x = (self.actual_width - scaled_w) // 2
            y = (self.actual_height - scaled_h) // 2
            if scaled_w > self.actual_width or scaled_h > self.actual_height:
                crop_l = max(0, -x)
                crop_t = max(0, -y)
                crop_r = crop_l + min(scaled_w, self.actual_width)
                crop_b = crop_t + min(scaled_h, self.actual_height)
                scaled = scaled.crop((crop_l, crop_t, crop_r, crop_b))
                x = max(0, x)
                y = max(0, y)
            canvas.paste(scaled, (x, y))

        if self.rotation in (90, 180, 270):
            # PIL rotates counter-clockwise, so negate the configured
            # clockwise rotation. expand=False keeps the canvas size
            # locked to the framebuffer geometry.
            canvas = canvas.rotate(-self.rotation, expand=False)

        if self.bpp == 16:
            buf = _pack_rgb565(canvas)
        elif self.bpp == 24:
            buf = canvas.tobytes()
        else:  # 32 bpp xRGB
            rgb = canvas.tobytes()
            out = bytearray(len(rgb) // 3 * 4)
            for i in range(0, len(rgb), 3):
                out[(i // 3) * 4 + 0] = rgb[i + 2]  # B
                out[(i // 3) * 4 + 1] = rgb[i + 1]  # G
                out[(i // 3) * 4 + 2] = rgb[i + 0]  # R
                out[(i // 3) * 4 + 3] = 0
            buf = bytes(out)

        if len(buf) != self._frame_bytes:
            log.warning(
                "framebuffer_size_mismatch",
                produced=len(buf),
                expected=self._frame_bytes,
            )
            return None
        return buf

    def _writer_loop(self) -> None:
        """Daemon-thread SPI writer. Pulls from _pending_image, writes."""
        import time as _time

        while True:
            stop = self._writer_stop.is_set()
            # Wait for a fresh frame, but wake periodically so cleanup()
            # can break us out without relying on the event being set.
            if not stop:
                self._pending_event.wait(timeout=0.5)
            self._pending_event.clear()
            with self._pending_lock:
                image = self._pending_image
                self._pending_image = None
            if image is None:
                # No pending work. If we were asked to stop, exit now;
                # otherwise loop back to wait for the next frame.
                if stop:
                    return
                continue
            # If stop is set but we have a pending image, drain it
            # before exiting so a present() right before cleanup()
            # actually lands on the framebuffer (tests rely on this).
            # Fast-skip when the input bytes are identical to the last
            # successfully written frame. The hash is over the raw input
            # image bytes, not the packed buffer, so we avoid the
            # expensive pure-Python RGB565 pack entirely on duplicates.
            try:
                input_hash = hash(image.tobytes())
            except Exception:  # noqa: BLE001
                input_hash = None
            if (
                input_hash is not None
                and self._last_written_hash is not None
                and input_hash == self._last_written_hash
            ):
                self._writer_skipped_duplicates += 1
                continue
            try:
                buf = self._compose_and_pack(image)
            except Exception as exc:  # noqa: BLE001
                log.warning("framebuffer_compose_failed", error=str(exc))
                continue
            if buf is None:
                continue
            mmap_obj = self._mmap
            if mmap_obj is None:
                # Renderer torn down between stash and write.
                return
            t0 = _time.monotonic()
            try:
                mmap_obj.seek(0)
                mmap_obj.write(buf)
            except (OSError, ValueError) as exc:
                # ValueError on closed mmap during cleanup; OSError on
                # disconnected SPI bus. Either way, stop trying.
                log.warning("framebuffer_write_failed", error=str(exc))
                return
            self._last_write_ms = (_time.monotonic() - t0) * 1000.0
            self._writer_writes += 1
            if input_hash is not None:
                self._last_written_hash = input_hash

    def stats(self) -> dict:
        """Return SPI-writer observability snapshot."""
        return {
            "writes": self._writer_writes,
            "drops": self._writer_drops,
            "skipped_duplicates": self._writer_skipped_duplicates,
            "last_write_ms": (
                round(self._last_write_ms, 2)
                if self._last_write_ms is not None
                else None
            ),
        }

    def cleanup(self) -> None:
        # Stop the writer thread before closing the mmap; otherwise it
        # could try to write into a closed mapping and crash.
        self._writer_stop.set()
        self._pending_event.set()
        thread = self._writer_thread
        if thread is not None and thread.is_alive():
            try:
                thread.join(timeout=1.0)
            except Exception:  # noqa: BLE001
                pass
        if self._mmap is not None:
            try:
                self._mmap.close()
            except Exception:  # noqa: BLE001
                pass
            self._mmap = None
        if self._fd >= 0:
            try:
                os.close(self._fd)
            except OSError:
                pass
            self._fd = -1
