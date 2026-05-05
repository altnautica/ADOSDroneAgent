"""Filled-circle status dot.

Used in the header (role indicator) and inside tiles (mesh / uplink
status). Color is the only signal — no shape variation for now since
we have three-color status semantics that map cleanly to
success/warning/error.
"""

from __future__ import annotations

from PIL import Image, ImageDraw

from . import primitives as p


def draw_dot(
    image: Image.Image,
    cx: int,
    cy: int,
    color: tuple[int, int, int],
    radius: int = 7,
) -> None:
    """Draw a filled circle centered on ``(cx, cy)``.

    Default 14 px diameter (radius 7) matches the in-tile dot size
    spec'd in the dashboard layout. The header role indicator uses
    radius 9 (18 px diameter) instead — pass ``radius=9``.
    """
    draw = ImageDraw.Draw(image)
    draw.ellipse(
        (cx - radius, cy - radius, cx + radius, cy + radius),
        fill=color,
        outline=p.BG_PRIMARY,  # 1 px black hairline so the dot pops on dark tiles
    )
