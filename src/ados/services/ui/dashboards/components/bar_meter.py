"""Segmented horizontal bar meter (used for radio bitrate fill).

Renders ``segments`` filled-or-empty chips left-to-right. The fill
fraction is the achieved value over the cap; the meter clamps the
fill at 100% so a momentary overshoot doesn't break the layout.

This is deliberately discrete (5-7 segments) rather than a smooth
gradient bar. At 480x320 with no anti-aliasing, discrete chips read
faster from across the room than a continuous fill would.
"""

from __future__ import annotations

from PIL import Image, ImageDraw

from . import primitives as p


def draw_bar(
    image: Image.Image,
    x: int,
    y: int,
    w: int,
    h: int,
    fraction: float | None,
    *,
    segments: int = 5,
    fill_color: tuple[int, int, int] = p.STATUS_SUCCESS,
    empty_color: tuple[int, int, int] = p.BORDER_STRONG,
    gap: int = 2,
) -> None:
    """Paint a chipped bar.

    ``fraction`` of None or below 0 yields an all-empty bar; above 1
    yields all-filled. Each chip is ``(w - (segments-1)*gap) /
    segments`` wide. Chips are filled left-to-right based on
    fraction * segments.
    """
    if fraction is None:
        fraction = 0.0
    fraction = max(0.0, min(1.0, fraction))
    filled_count = int(round(fraction * segments))

    chip_w = (w - (segments - 1) * gap) / segments
    draw = ImageDraw.Draw(image)
    for i in range(segments):
        cx = x + int(round(i * (chip_w + gap)))
        c_w = max(1, int(round(chip_w)))
        color = fill_color if i < filled_count else empty_color
        draw.rectangle((cx, y, cx + c_w - 1, y + h - 1), fill=color)
