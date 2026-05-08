"""More page — overflow menu for the bottom tab bar's "+" tab.

The bottom tab bar has four primary tabs (Dashboard, Video, Settings,
More). The More page is a short scrollable list of secondary actions
that don't fit anywhere else but are still operator-accessible without
hopping back to the GCS:

* **Pair drone** — drilldown to the pair drone detail page (paired
  view shows device id + key fingerprint + Unpair button; unpaired
  view shows the pairing code + QR + Open-pairing-window button).
* **Diagnostics** — drilldown to a system-info page (CPU / RAM / temp
  with sparklines, agent + board identity, last 10 journal lines).
* **Restart agent** — confirm dialog → POST the supervisor restart
  endpoint. The supervisor brings the agent back up after a few
  seconds; the LCD reflects that via the heartbeat banner.
* **About** — drilldown to the existing AboutPage detail (read-only
  build identity and license info).

Layout: 4 rows of 48 px each. The page reuses :func:`draw_list_row`
from the settings widgets so visual styling is consistent. There is
no scroll envelope here because four rows fit inside the 244 px
content area (4 * 48 = 192 px, with 52 px of headroom).
"""

from __future__ import annotations

from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from typing import ClassVar

from PIL import Image

from ados.services.ui.pages.base import HitZone, PageContext
from ados.services.ui.touch.events import TouchGesture
from ados.services.ui.widgets import ROW_H, ConfirmDialog, draw_list_row

PAGE_W = 480
PAGE_H = 244


@dataclass(frozen=True)
class _Row:
    """One More-page row.

    ``id`` is the stable hit-zone suffix and dispatch key. ``label`` is
    the operator-facing copy. ``handler`` is the coroutine fired on
    tap.
    """

    id: str
    label: str
    handler: Callable[[MorePage, PageContext, _Row], Awaitable[None]]


class MorePage:
    """The "+" tab content — drilldown menu for secondary actions."""

    id: ClassVar[str] = "more"
    refresh_hz: ClassVar[float] = 5.0

    def __init__(self) -> None:
        self._rows: tuple[_Row, ...] = _ROW_DEFS

    # ── lifecycle ──────────────────────────────────────────────

    async def on_enter(self, ctx: PageContext) -> None:
        ctx.logger.info("more_enter")

    async def on_leave(self, ctx: PageContext) -> None:
        ctx.logger.info("more_leave")

    # ── render ─────────────────────────────────────────────────

    async def render(self, ctx: PageContext) -> Image.Image:
        palette = ctx.palette
        img = Image.new("RGB", (PAGE_W, PAGE_H), palette.bg_primary)
        for i, row in enumerate(self._rows):
            row_y = i * ROW_H
            draw_list_row(
                img,
                0,
                row_y,
                PAGE_W,
                palette=palette,
                label=row.label,
                value=None,
                variant="default",
                state=None,
            )
        return img

    # ── hit zones + dispatch ───────────────────────────────────

    def hit_zones(self, ctx: PageContext) -> list[HitZone]:
        zones: list[HitZone] = []
        for i, row in enumerate(self._rows):
            zones.append(
                HitZone(
                    id=f"row:{row.id}",
                    x=0,
                    y=i * ROW_H,
                    w=PAGE_W,
                    h=ROW_H,
                )
            )
        return zones

    async def on_touch(
        self,
        ctx: PageContext,
        zone: HitZone,
        gesture: TouchGesture,
    ) -> None:
        if gesture.kind != "tap":
            return
        if not zone.id.startswith("row:"):
            return
        row_id = zone.id.removeprefix("row:")
        row = self._row_for(row_id)
        if row is None:
            return
        try:
            await row.handler(self, ctx, row)
        except Exception as exc:  # noqa: BLE001
            ctx.logger.warning(
                "more_row_handler_failed",
                row=row.id,
                error=str(exc),
            )

    def _row_for(self, row_id: str) -> _Row | None:
        for r in self._rows:
            if r.id == row_id:
                return r
        return None


# ── handlers ───────────────────────────────────────────────────


async def _pair_drilldown(
    page: MorePage, ctx: PageContext, row: _Row,
) -> None:
    from ados.services.ui.pages.details.pair_drone import PairDroneDetailPage

    await ctx.navigator.push_modal(PairDroneDetailPage(), ctx=ctx)


async def _diagnostics_drilldown(
    page: MorePage, ctx: PageContext, row: _Row,
) -> None:
    from ados.services.ui.pages.details.diagnostics import (
        DiagnosticsDetailPage,
    )

    await ctx.navigator.push_modal(DiagnosticsDetailPage(), ctx=ctx)


async def _restart_action(
    page: MorePage, ctx: PageContext, row: _Row,
) -> None:
    async def _on_confirm() -> None:
        client = ctx.http
        if client is None:
            ctx.logger.warning("more_restart_no_http")
            return
        try:
            r = await client.post(
                "/api/v1/system/restart-supervisor",
                timeout=2.0,
            )
            if 200 <= r.status_code < 300:
                ctx.logger.info("more_restart_dispatched")
            else:
                ctx.logger.warning(
                    "more_restart_rejected",
                    status=r.status_code,
                )
        except Exception as exc:  # noqa: BLE001
            # The agent process is killed by the supervisor mid-call,
            # so a transport error here is the success signal — log it
            # at debug level so it doesn't read as a real failure.
            ctx.logger.debug(
                "more_restart_request_dropped",
                error=str(exc),
            )

    await ctx.navigator.push_modal(
        ConfirmDialog(
            "Restart agent",
            (
                "Restart the ADOS agent service. The LCD goes black for "
                "a few seconds while the supervisor brings it back up."
            ),
            confirm_label="Restart",
            confirm_destructive=False,
            on_confirm=_on_confirm,
        ),
        ctx=ctx,
    )


async def _about_drilldown(
    page: MorePage, ctx: PageContext, row: _Row,
) -> None:
    from ados.services.ui.pages.details.about import AboutPage

    await ctx.navigator.push_modal(AboutPage(), ctx=ctx)


# ── row registry ───────────────────────────────────────────────

_ROW_DEFS: tuple[_Row, ...] = (
    _Row("more.row.pair", "Pair drone", _pair_drilldown),
    _Row("more.row.diagnostics", "Diagnostics", _diagnostics_drilldown),
    _Row("more.row.restart", "Restart agent", _restart_action),
    _Row("more.row.about", "About", _about_drilldown),
)
