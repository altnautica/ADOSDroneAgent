"""Video frame compositor for the LCD video page.

A thin helper that holds the most recent decoded RGB frame and blits
it into a caller-supplied PIL canvas at a given origin. The compositor
itself does no decoding — the page's :class:`LocalVideoTap` callback
calls :meth:`set` from the gstreamer thread, the page's render path
calls :meth:`paint` from the asyncio loop. The single-slot lock lives
inside the compositor so the two threads can hand off frames without
an explicit queue.

When no frame has arrived yet (cold start, RTSP unreachable, decoder
failure), :meth:`paint` falls back to a centered "Video link not
available" message in the active palette so the operator never sees a
black hole on the page.
"""

from __future__ import annotations

import threading

from PIL import Image, ImageDraw

from ados.services.ui.dashboards.components import primitives as p
from ados.services.ui.theme import Palette


class VideoCompositor:
    """Threadsafe latest-frame holder with a paint-onto-canvas helper."""

    def __init__(self) -> None:
        self._frame: Image.Image | None = None
        self._lock = threading.Lock()

    def set(self, frame: Image.Image | None) -> None:
        """Store ``frame`` as the latest decoded image (single-slot)."""
        with self._lock:
            self._frame = frame

    def latest(self) -> Image.Image | None:
        """Return the most recently stored frame, or ``None``."""
        with self._lock:
            return self._frame

    def paint(
        self,
        canvas: Image.Image,
        x: int,
        y: int,
        *,
        palette: Palette,
        frame: Image.Image | None,
        width: int = 480,
        height: int = 176,
        message: str = "Video link not available",
    ) -> None:
        """Blit ``frame`` onto ``canvas`` at ``(x, y)``.

        When ``frame`` is None we paint a neutral placeholder card with
        the supplied ``message`` centered in the region. The card uses
        ``bg_secondary`` so it reads as a dedicated video plane even
        before the first decoded frame arrives.
        """
        if frame is not None:
            try:
                if frame.size != (width, height):
                    # Defensive: a future caller could pass a frame at
                    # an off-spec resolution. Resize on the page's
                    # behalf so we never paste outside the region.
                    frame = frame.resize((width, height))
                if frame.mode != "RGB":
                    frame = frame.convert("RGB")
                canvas.paste(frame, (x, y))
                return
            except Exception:
                # Fall through to the placeholder below.
                pass

        draw = ImageDraw.Draw(canvas)
        draw.rectangle(
            (x, y, x + width - 1, y + height - 1),
            fill=palette.bg_secondary,
        )
        font = p.font("sans_regular", 14)
        text_w, text_h = p.text_size(canvas, message, font)
        tx = x + (width - text_w) // 2
        ty = y + (height - text_h) // 2
        draw.text((tx, ty), message, fill=palette.text_secondary, font=font)
