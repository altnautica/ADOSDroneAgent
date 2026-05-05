"""Tiny line-graph sparkline + ring buffer for system metrics.

The footer shows the current CPU/temp/RAM as numbers, but a number
alone doesn't convey trend. A 60-sample sparkline next to each
number lets the operator see at a glance that the box is "settling
in" vs "climbing toward thermal limit" — same data, more meaning.

Buffer strategy: a single module-level dict keyed by metric name,
each entry a fixed-length deque. The dashboard renderer's tick
appends current values; the sparkline renderer reads from the
deque. No I/O, no thread safety needed (single-threaded asyncio).
"""

from __future__ import annotations

from collections import deque
from typing import Deque

from PIL import Image, ImageDraw

from . import primitives as p


# 60 seconds @ 1 Hz polling = 60 samples
HISTORY_LEN = 60

_history: dict[str, Deque[float | None]] = {}


def push(metric: str, value: float | None) -> None:
    """Record one sample of a named metric. Caller decides the units."""
    buf = _history.get(metric)
    if buf is None:
        buf = deque(maxlen=HISTORY_LEN)
        _history[metric] = buf
    buf.append(value)


def history(metric: str) -> list[float | None]:
    return list(_history.get(metric, ()))


def draw_sparkline(
    image: Image.Image,
    x: int,
    y: int,
    w: int,
    h: int,
    samples: list[float | None],
    *,
    color: tuple[int, int, int] = p.TEXT_SECONDARY,
    fill_below: bool = False,
    y_min: float | None = None,
    y_max: float | None = None,
) -> None:
    """Render ``samples`` as a 1-pixel polyline inside ``(x, y, w, h)``.

    None samples create a gap in the line — clear visual signal that
    a metric was unavailable rather than zero. Auto-scales to the
    sample range unless ``y_min`` / ``y_max`` are pinned (useful for
    metrics with a fixed scale like CPU 0-100).
    """
    if not samples:
        return
    draw = ImageDraw.Draw(image)

    # Determine y range. Drop Nones for the auto-scale; if everything
    # is None, render nothing.
    real = [s for s in samples if s is not None]
    if not real:
        return
    lo = y_min if y_min is not None else min(real)
    hi = y_max if y_max is not None else max(real)
    if hi <= lo:
        hi = lo + 1.0  # avoid div-by-zero on flat-line buffers

    # Map samples to pixels.
    n = len(samples)
    if n < 2:
        return
    xs = [x + int(round(i * (w - 1) / (n - 1))) for i in range(n)]

    def _y_for(value: float | None) -> int | None:
        if value is None:
            return None
        # Invert y because PIL has origin at top-left.
        clamped = max(lo, min(hi, value))
        frac = (clamped - lo) / (hi - lo)
        return y + h - 1 - int(round(frac * (h - 1)))

    ys = [_y_for(s) for s in samples]

    # Draw connected segments where both endpoints are non-None.
    for i in range(n - 1):
        y0 = ys[i]
        y1 = ys[i + 1]
        if y0 is None or y1 is None:
            continue
        draw.line((xs[i], y0, xs[i + 1], y1), fill=color, width=1)

    if fill_below:
        # Faint area-fill under the line. Cosmetic.
        for i, sy in enumerate(ys):
            if sy is None:
                continue
            draw.line((xs[i], sy + 1, xs[i], y + h - 1), fill=color)
