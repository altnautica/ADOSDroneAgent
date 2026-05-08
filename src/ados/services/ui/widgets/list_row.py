"""48 px scrollable list row primitive used by the settings page.

A row is a thin functional renderer: it paints into a caller-provided
``Image`` and returns nothing. The settings page wraps each draw in
its own clipped band and translates the y origin so the row lives at
the right place inside a scrollable list. A separate
:class:`HitZone` is created by the caller from the same geometry it
passed in here.

Variants
--------

* ``default`` — label + optional value + chevron. Used for drilldowns
  and editor-row entry points.
* ``toggle`` — label + 36x20 switch. ``state`` is ``True`` / ``False``.
* ``action`` — label only, no chevron. ``state`` may be a short status
  string painted in muted tone (e.g. "running"). Used for one-shot
  actions like Reboot or Calibrate.

The divider line is drawn as a 1 px ``border_default`` strip across
the bottom of the row when ``divider_below=True`` so a contiguous list
reads as a single scrollable surface without gaps.
"""

from __future__ import annotations

from typing import Any

from PIL import Image, ImageDraw

from ados.services.ui.dashboards.components import primitives as p
from ados.services.ui.theme import Palette

ROW_H = 48

# Switch geometry for the toggle variant.
_SWITCH_W = 36
_SWITCH_H = 20
_SWITCH_PAD = 12

# Pixel padding from the right edge for the chevron / switch.
_RIGHT_PAD = 12

# Pixel padding from the left edge for the icon / label.
_LEFT_PAD = 12


def draw_list_row(
    image: Image.Image,
    x: int,
    y: int,
    w: int,
    *,
    palette: Palette,
    label: str,
    value: str | None = None,
    icon_name: str | None = None,
    variant: str = "default",
    state: Any = None,
    divider_below: bool = True,
) -> None:
    """Paint a 48 px row into ``image`` at the given origin.

    ``label`` is the primary copy. ``value`` is the optional secondary
    copy (current setting in mono 12 grey, right-aligned but inside
    the chevron). ``icon_name`` is a 22x22 glyph name (optional;
    centered vertically at the left). ``variant`` selects the layout.
    ``state`` carries variant-specific data (bool for toggle, status
    string for action). ``divider_below`` paints a 1 px border line
    under the row so a stack of rows reads as a list.

    Coordinates are in the ``image``'s own space; the caller has
    already done any scroll translation.
    """
    if variant not in ("default", "toggle", "action"):
        variant = "default"

    draw = ImageDraw.Draw(image)
    h = ROW_H
    label_x = x + _LEFT_PAD
    if icon_name:
        # Icons are 22 px and live at the very left, vertically centered.
        # We don't ship a stable icon set yet for settings rows; the
        # caller passes the name and we paint a small filled square
        # placeholder so the layout reserves the space. When a proper
        # glyph drop lands the helper switches to the real bitmap.
        icon_x = x + _LEFT_PAD
        icon_y = y + (h - 22) // 2
        draw.rectangle(
            (icon_x, icon_y, icon_x + 21, icon_y + 21),
            fill=palette.bg_tertiary,
            outline=palette.border_default,
            width=1,
        )
        label_x = icon_x + 22 + 8

    # Label sits centered vertically. DejaVu Sans 14, text_primary.
    label_font = p.font("sans_regular", 14)
    _, label_h = p.text_size(image, label, label_font)
    label_y = y + (h - label_h) // 2 - 2
    draw.text((label_x, label_y), label, fill=palette.text_primary, font=label_font)

    if variant == "toggle":
        _draw_switch(image, x, y, w, palette=palette, on=bool(state))
    elif variant == "action":
        if isinstance(state, str) and state:
            status_font = p.font("mono_regular", 11)
            sw, sh = p.text_size(image, state, status_font)
            sx = x + w - _RIGHT_PAD - sw
            sy = y + (h - sh) // 2 - 1
            draw.text((sx, sy), state, fill=palette.text_tertiary, font=status_font)
    else:
        # default: optional value text + chevron arrow.
        chevron_x = x + w - _RIGHT_PAD - 8
        if value is not None and value != "":
            value_font = p.font("mono_regular", 12)
            vw, vh = p.text_size(image, value, value_font)
            vx = chevron_x - 12 - vw
            vy = y + (h - vh) // 2 - 1
            draw.text(
                (vx, vy),
                value,
                fill=palette.text_secondary,
                font=value_font,
            )
        _draw_chevron(draw, chevron_x, y + h // 2, palette.text_tertiary)

    if divider_below:
        draw.line(
            (x, y + h - 1, x + w - 1, y + h - 1),
            fill=palette.border_default,
        )


def _draw_chevron(
    draw: ImageDraw.ImageDraw,
    cx: int,
    cy: int,
    color: tuple[int, int, int],
) -> None:
    """Draw a right-pointing chevron centered on ``(cx, cy)``."""
    arm = 5
    draw.line((cx - arm, cy - arm, cx, cy), fill=color, width=2)
    draw.line((cx, cy, cx - arm, cy + arm), fill=color, width=2)


def _draw_switch(
    image: Image.Image,
    x: int,
    y: int,
    w: int,
    *,
    palette: Palette,
    on: bool,
) -> None:
    """Draw a 36x20 toggle switch right-aligned to the row."""
    draw = ImageDraw.Draw(image)
    sx = x + w - _RIGHT_PAD - _SWITCH_W
    sy = y + (ROW_H - _SWITCH_H) // 2
    bg = palette.accent_primary if on else palette.bg_tertiary
    border = palette.accent_primary if on else palette.border_strong
    # Pillow rounded_rectangle landed in 8.2; we paint a plain rect
    # which renders crisp at 480x320 and matches the mockups. The
    # outline gives the off-state a defined border so it doesn't
    # disappear on dark backgrounds.
    draw.rectangle(
        (sx, sy, sx + _SWITCH_W - 1, sy + _SWITCH_H - 1),
        fill=bg,
        outline=border,
        width=1,
    )
    # Knob: 16x16 white circle inset by 2 px on the active side.
    knob_d = _SWITCH_H - 4
    knob_y = sy + 2
    if on:
        knob_x = sx + _SWITCH_W - knob_d - 2
    else:
        knob_x = sx + 2
    draw.ellipse(
        (knob_x, knob_y, knob_x + knob_d, knob_y + knob_d),
        fill=palette.text_primary,
    )
