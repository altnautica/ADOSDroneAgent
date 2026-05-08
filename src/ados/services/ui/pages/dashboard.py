"""Placeholder Dashboard page.

The full 4-tile refit lands in the next commit. This page exists in
C2 so the page system is exercisable end-to-end on real hardware:
the navigator can go("dashboard"), the chrome paints around it, and
tile-quadrant taps reach :meth:`on_touch` and log structurally.

Layout: a 480x244 panel split into four 240x122 tile zones. Each tap
on a quadrant logs a ``dashboard_quadrant_tap`` event with the zone
id so an integrator can verify hit-test plumbing on the bench
without waiting for the full tile renderers to ship.
"""

from __future__ import annotations

from typing import ClassVar

from PIL import Image, ImageDraw

from ados.services.ui.dashboards.components import primitives as p
from ados.services.ui.touch.events import TouchGesture

from .base import HitZone, PageContext

PAGE_W = 480
PAGE_H = 244


class DashboardPage:
    """4-quadrant placeholder page registered as ``dashboard``."""

    id: ClassVar[str] = "dashboard"
    refresh_hz: ClassVar[float] = 5.0

    _ZONES: ClassVar[tuple[tuple[str, int, int, int, int], ...]] = (
        ("dashboard.tile.radio", 0, 0, 240, 122),
        ("dashboard.tile.drone", 240, 0, 240, 122),
        ("dashboard.tile.mesh", 0, 122, 240, 122),
        ("dashboard.tile.uplink", 240, 122, 240, 122),
    )

    async def on_enter(self, ctx: PageContext) -> None:
        ctx.logger.info("dashboard_enter")

    async def on_leave(self, ctx: PageContext) -> None:
        ctx.logger.info("dashboard_leave")

    async def render(self, ctx: PageContext) -> Image.Image:
        palette = ctx.palette
        img = Image.new("RGB", (PAGE_W, PAGE_H), palette.bg_primary)
        draw = ImageDraw.Draw(img)

        # Outline the four tile zones in border_default so the
        # quadrants are visually obvious during bring-up.
        for _, x, y, w, h in self._ZONES:
            draw.rectangle(
                (x + 2, y + 2, x + w - 2, y + h - 2),
                outline=palette.border_default,
                width=1,
            )

        title_font = p.font("sans_bold", 18)
        body_font = p.font("sans_regular", 12)
        title_text = "Dashboard"
        body_text = "Tile detail renderers land in the next commit."
        title_w, _ = p.text_size(img, title_text, title_font)
        body_w, _ = p.text_size(img, body_text, body_font)
        draw.text(
            ((PAGE_W - title_w) // 2, PAGE_H // 2 - 22),
            title_text,
            fill=palette.text_primary,
            font=title_font,
        )
        draw.text(
            ((PAGE_W - body_w) // 2, PAGE_H // 2 + 4),
            body_text,
            fill=palette.text_secondary,
            font=body_font,
        )
        return img

    def hit_zones(self, ctx: PageContext) -> list[HitZone]:
        return [
            HitZone(id=zid, x=x, y=y, w=w, h=h)
            for zid, x, y, w, h in self._ZONES
        ]

    async def on_touch(
        self,
        ctx: PageContext,
        zone: HitZone,
        gesture: TouchGesture,
    ) -> None:
        ctx.logger.info(
            "dashboard_quadrant_tap",
            zone_id=zone.id,
            kind=gesture.kind,
            x=gesture.start_x,
            y=gesture.start_y,
        )
