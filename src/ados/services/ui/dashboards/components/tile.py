"""Generic tile primitive — bordered box with title bar + body slot.

Each content tile on the dashboard is a tile() call: a 1 px border
box filled with bg-secondary, a title row in muted caps at the top,
and a body region the caller paints into. The title row stays
consistent across tiles so the eye can scan all four boxes at once
without re-anchoring.
"""

from __future__ import annotations

from PIL import Image, ImageDraw

from . import primitives as p

TITLE_PAD_X = 8
TITLE_BAR_H = 18  # height reserved for the caps title at top of tile


def draw_tile(
    image: Image.Image,
    x: int,
    y: int,
    w: int,
    h: int,
    title: str,
    *,
    title_right: str = "",
    fill: tuple[int, int, int] = p.BG_SECONDARY,
    border: tuple[int, int, int] = p.BORDER_DEFAULT,
) -> tuple[int, int, int, int]:
    """Paint the tile shell and return the inner ``(bx, by, bw, bh)`` body box.

    The caller paints further content into the returned bounds. The
    title bar lives in the top ``TITLE_BAR_H`` pixels; the body
    starts immediately below with 8 px inner padding on the sides
    and bottom. The caller decides body padding-top.
    """
    draw = ImageDraw.Draw(image)
    # Filled box.
    draw.rectangle((x, y, x + w - 1, y + h - 1), fill=fill, outline=border, width=1)
    # Title bar text — small uppercase letters, muted secondary color.
    title_font = p.font("sans_bold", 11)
    draw.text(
        (x + TITLE_PAD_X, y + 4),
        title.upper(),
        fill=p.TEXT_SECONDARY,
        font=title_font,
    )
    if title_right:
        right_font = p.font("mono_regular", 11)
        rw, _ = p.text_size(image, title_right, right_font)
        draw.text(
            (x + w - TITLE_PAD_X - rw, y + 4),
            title_right,
            fill=p.TEXT_TERTIARY,
            font=right_font,
        )
    # 1 px separator under the title bar.
    sep_y = y + TITLE_BAR_H
    draw.line((x + 1, sep_y, x + w - 2, sep_y), fill=p.BORDER_DEFAULT)

    # Body box.
    body_x = x + 8
    body_y = sep_y + 2
    body_w = w - 16
    body_h = h - TITLE_BAR_H - 10
    return body_x, body_y, body_w, body_h
