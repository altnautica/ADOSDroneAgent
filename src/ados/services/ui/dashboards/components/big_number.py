"""Large numeric value with threshold-based color + unit suffix.

The dashboard uses big numbers for the headline in each tile (RSSI,
battery percent, CPU percent). They share a common look: monospace
bold, readable from across the bench, color follows
``threshold_color()``.
"""

from __future__ import annotations

from PIL import Image, ImageDraw

from . import primitives as p


def draw_big_number(
    image: Image.Image,
    x: int,
    y: int,
    value_text: str,
    *,
    color: tuple[int, int, int] = p.TEXT_PRIMARY,
    size: int = 32,
    unit: str = "",
    unit_color: tuple[int, int, int] | None = None,
) -> int:
    """Paint the headline number, return total width painted (px).

    Letting callers know the painted width lets them lay out a
    secondary chip / icon to the right without re-measuring.

    ``unit`` (e.g. "dBm", "%") renders smaller and one weight down
    immediately to the right of the value, baseline-aligned. Passing
    an empty string skips the unit.
    """
    draw = ImageDraw.Draw(image)
    value_font = p.font("mono_bold", size)
    draw.text((x, y), value_text, fill=color, font=value_font)

    # Width of the value alone — used both for return + unit placement.
    value_w, value_h = p.text_size(image, value_text, value_font)
    total_w = value_w

    if unit:
        unit_font = p.font("sans_bold", max(10, size // 3))
        unit_w, unit_h = p.text_size(image, unit, unit_font)
        # Baseline-align unit to value baseline. Value height is
        # roughly the cap height of the font; nudging unit y by a
        # few px lands it sensibly across font sizes.
        unit_x = x + value_w + 4
        unit_y = y + value_h - unit_h - 2
        draw.text(
            (unit_x, unit_y),
            unit,
            fill=unit_color or p.TEXT_SECONDARY,
            font=unit_font,
        )
        total_w += 4 + unit_w

    return total_w
