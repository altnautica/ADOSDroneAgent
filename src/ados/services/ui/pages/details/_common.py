"""Shared header strip + back chevron for all drilldown detail pages.

Every detail page paints the same 40 px header band: a back chevron
on the left at (8, 8, 40, 32) and a centered uppercase title in
DejaVu Sans Bold 14. Centralising the helper here keeps the four
detail pages from diverging in chevron geometry or title styling.

Hit zone returned by :func:`draw_back_button` is the back chevron
zone. The page's ``hit_zones()`` should include it so a tap there
pops the modal.
"""

from __future__ import annotations

from PIL import Image, ImageDraw

from ados.services.ui.dashboards.components import primitives as p
from ados.services.ui.pages.base import HitZone
from ados.services.ui.theme import Palette

HEADER_H = 40
BACK_ZONE_X = 8
BACK_ZONE_Y = 8
BACK_ZONE_W = 40
BACK_ZONE_H = 32

BACK_ZONE_ID = "details.back"


def draw_back_button(image: Image.Image, palette: Palette) -> HitZone:
    """Paint the back chevron and return its hit zone.

    The chevron is two diagonal strokes meeting at a left-pointing
    apex inside a 40x32 box. Strokes are 2 px to read on a 480x244
    panel without becoming a thick wedge that dominates the header.
    Returns the matching :class:`HitZone` so the caller can include
    it in its ``hit_zones()`` list.
    """
    draw = ImageDraw.Draw(image)
    cx = BACK_ZONE_X + BACK_ZONE_W // 2
    cy = BACK_ZONE_Y + BACK_ZONE_H // 2
    arm = 8  # half-length of each chevron arm
    # The apex (cx - arm, cy) and two arms going up-right and
    # down-right form a left-pointing chevron.
    color = palette.text_primary
    draw.line((cx - arm, cy, cx + arm, cy - arm), fill=color, width=2)
    draw.line((cx - arm, cy, cx + arm, cy + arm), fill=color, width=2)
    return HitZone(
        id=BACK_ZONE_ID,
        x=BACK_ZONE_X,
        y=BACK_ZONE_Y,
        w=BACK_ZONE_W,
        h=BACK_ZONE_H,
    )


def draw_title(image: Image.Image, palette: Palette, title: str) -> None:
    """Paint a centered uppercase title at y=12 in DejaVu Sans Bold 14."""
    draw = ImageDraw.Draw(image)
    font = p.font("sans_bold", 14)
    text = title.upper()
    text_w, _ = p.text_size(image, text, font)
    img_w = image.size[0]
    draw.text(
        ((img_w - text_w) // 2, 12),
        text,
        fill=palette.text_primary,
        font=font,
    )


def draw_header_band(
    image: Image.Image,
    palette: Palette,
    title: str,
) -> HitZone:
    """Paint the back chevron + title and a 1 px divider under them.

    Returns the back-chevron hit zone. Pages call this once at the
    top of ``render`` and append the returned zone to whatever extra
    zones they expose.
    """
    back_zone = draw_back_button(image, palette)
    draw_title(image, palette, title)
    draw = ImageDraw.Draw(image)
    img_w = image.size[0]
    # 1 px divider just under the header band so the body content
    # reads as visually separated.
    draw.line(
        (0, HEADER_H, img_w - 1, HEADER_H),
        fill=palette.border_default,
    )
    return back_zone
