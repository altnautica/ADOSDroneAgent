"""Bottom tab bar — 44 px persistent navigation strip.

Four tabs each 120 px wide laid out left-to-right: Dashboard, Video,
Settings, More (the plus glyph). Each tab has:

* A tinted 24 px icon (``text_tertiary`` for inactive, ``text_primary``
  for active, palette-inverted while a tap-feedback fill is rendering).
* A 2 px top accent line in ``accent_primary`` over the active tab.
* An inverse-fill flash that lasts roughly one render tick after the
  operator taps it, surfaced via ``tapped_at_ms``.

The function returns the four hit zones so the page navigator can
dispatch tap gestures back to a tab without redoing the layout math.
"""

from __future__ import annotations

import time

from PIL import Image, ImageDraw

from ados.services.ui.chrome.icons import get_icon, tint
from ados.services.ui.pages.base import HitZone
from ados.services.ui.theme import Palette

HEIGHT = 44
TAB_WIDTH = 120
TAB_COUNT = 4

# Stable id per tab. The page id is the same as the navigator route.
_TABS: tuple[tuple[str, str, str], ...] = (
    # (zone_id, page_id, icon_name)
    ("tab.dashboard", "dashboard", "dashboard"),
    ("tab.video", "video", "video"),
    ("tab.settings", "settings", "settings"),
    ("tab.more", "more", "plus"),
)


# How long a tap-feedback inverse-fill lingers. The render loop
# typically ticks at 5 Hz on the dashboard so a 200 ms flash overlaps
# 1-2 frames; on the 20 Hz video page it overlaps 4 frames. Either
# way the operator sees a visible pulse without it leaving a stuck
# inverse forever if a callback hangs.
_FEEDBACK_LINGER_MS = 200


def _now_ms() -> int:
    return int(time.monotonic() * 1000)


def draw(
    image: Image.Image,
    x: int,
    y: int,
    w: int,
    *,
    palette: Palette,
    active: str,
    tapped_at_ms: dict[str, int] | None = None,
    now_ms: int | None = None,
) -> list[HitZone]:
    """Paint the tab bar in place and return the four hit zones.

    ``active`` is the page id (``dashboard``/``video``/``settings``/
    ``more``) of the currently visible page; the matching tab gets
    the active treatment. ``tapped_at_ms`` lets the navigator surface
    "this tab was just tapped at T ms" so the renderer paints the
    inverse-fill flash. ``now_ms`` overrides the live clock for
    deterministic snapshot tests.
    """
    draw_obj = ImageDraw.Draw(image)
    h = HEIGHT
    if now_ms is None:
        now_ms = _now_ms()

    # Solid background and 1 px divider above the bar.
    draw_obj.rectangle((x, y, x + w - 1, y + h - 1), fill=palette.bg_secondary)
    draw_obj.line((x, y, x + w - 1, y), fill=palette.border_default)

    zones: list[HitZone] = []
    feedback = tapped_at_ms or {}

    for i, (zone_id, page_id, icon_name) in enumerate(_TABS):
        tab_x0 = x + i * TAB_WIDTH
        tab_y0 = y
        tab_x1 = tab_x0 + TAB_WIDTH
        tab_y1 = tab_y0 + h

        is_active = page_id == active
        feedback_age = now_ms - feedback.get(zone_id, -10**9)
        is_tapping = 0 <= feedback_age <= _FEEDBACK_LINGER_MS

        if is_tapping:
            # Inverse fill — flip background to the active text color
            # so the operator sees a clear "I touched it" pulse. Icon
            # tinted to bg_primary keeps it readable on top of the
            # filled rectangle.
            draw_obj.rectangle(
                (tab_x0, tab_y0, tab_x1 - 1, tab_y1 - 1),
                fill=palette.text_primary,
            )
            icon_color = palette.bg_primary
        else:
            icon_color = palette.text_primary if is_active else palette.text_tertiary

        # 2 px top-edge accent line on the active tab.
        if is_active and not is_tapping:
            draw_obj.line(
                (tab_x0, tab_y0, tab_x1 - 1, tab_y0),
                fill=palette.accent_primary,
                width=1,
            )
            draw_obj.line(
                (tab_x0, tab_y0 + 1, tab_x1 - 1, tab_y0 + 1),
                fill=palette.accent_primary,
                width=1,
            )

        # Icon centered horizontally, vertically nudged 2 px below the
        # accent line to keep it visually grounded inside the tab.
        icon = tint(get_icon(icon_name), icon_color)
        ix = tab_x0 + (TAB_WIDTH - icon.width) // 2
        iy = tab_y0 + (h - icon.height) // 2
        image.paste(icon, (ix, iy), icon)

        zones.append(
            HitZone(
                id=zone_id,
                x=tab_x0,
                y=tab_y0,
                w=TAB_WIDTH,
                h=h,
            )
        )

    return zones


def page_id_for_zone(zone_id: str) -> str | None:
    """Return the page id ``zone_id`` routes to, or None if unknown."""
    for zid, pid, _ in _TABS:
        if zid == zone_id:
            return pid
    return None
