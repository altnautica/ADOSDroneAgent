"""Dashboard page — 4-tile live status grid + drilldown taps.

Renders the chrome-less 480x244 inset version of the groundnode
landscape dashboard and surfaces four hit zones (one per tile). A
tap on a tile pushes the matching detail page onto the navigator's
modal stack so the operator can drill in without losing the active
tab.

Tile geometry mirrors :func:`render_landscape_inset`:

  * outer margin 8 px on all sides
  * 8 px gap between tiles
  * tile size = (480 - 16 - 8) / 2 by (244 - 16 - 8) / 2 = 228 x 110 px

Hit-zone coordinates are page-local (y=0 is the top of the 480x244
content area, not the LCD-global y=32).
"""

from __future__ import annotations

from typing import ClassVar

from PIL import Image

from ados.services.ui.dashboards.groundnode_landscape import (
    render_landscape_inset,
)
from ados.services.ui.touch.events import TouchGesture

from .base import HitZone, PageContext

PAGE_W = 480
PAGE_H = 244


class DashboardPage:
    """Live 4-tile dashboard registered as ``dashboard``."""

    id: ClassVar[str] = "dashboard"
    refresh_hz: ClassVar[float] = 5.0

    # Hit zones in page-local coordinates. The values match the tile
    # rectangles laid out by ``render_landscape_inset`` so a tap on a
    # tile lands in its matching zone reliably across themes.
    _ZONES: ClassVar[tuple[tuple[str, int, int, int, int], ...]] = (
        ("tile.radio_link", 8, 8, 228, 110),
        ("tile.drone", 244, 8, 228, 110),
        ("tile.mesh", 8, 126, 228, 110),
        ("tile.uplink", 244, 126, 228, 110),
    )

    async def on_enter(self, ctx: PageContext) -> None:
        ctx.logger.info("dashboard_enter")

    async def on_leave(self, ctx: PageContext) -> None:
        ctx.logger.info("dashboard_leave")

    async def render(self, ctx: PageContext) -> Image.Image:
        return render_landscape_inset(
            ctx.state,
            ctx.hostname,
            palette=ctx.palette,
            width=PAGE_W,
            height=PAGE_H,
        )

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
        if gesture.kind != "tap":
            return
        # Local imports keep the page module load cheap and avoid the
        # detail-page modules pulling in the dashboard at import time.
        from .details.drone import DroneDetailPage
        from .details.mesh import MeshDetailPage
        from .details.radio_link import RadioLinkDetailPage
        from .details.uplink import UplinkDetailPage

        ctx.logger.info(
            "dashboard_tile_tap",
            zone_id=zone.id,
            x=gesture.start_x,
            y=gesture.start_y,
        )
        if zone.id == "tile.radio_link":
            await ctx.navigator.push_modal(RadioLinkDetailPage(), ctx=ctx)
        elif zone.id == "tile.drone":
            await ctx.navigator.push_modal(DroneDetailPage(), ctx=ctx)
        elif zone.id == "tile.mesh":
            await ctx.navigator.push_modal(MeshDetailPage(), ctx=ctx)
        elif zone.id == "tile.uplink":
            await ctx.navigator.push_modal(UplinkDetailPage(), ctx=ctx)
