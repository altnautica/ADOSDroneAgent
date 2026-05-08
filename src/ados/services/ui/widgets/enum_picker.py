"""Modal page that picks one value out of a list of options.

The settings page pushes one of these for any enum-style row (channel,
MCS index, topology, role, theme, etc.). The picker paints a header
band identical to the detail pages plus a scrollable list of 48 px
option rows. Tapping a row commits the chosen value via the supplied
``on_save`` callback then pops itself.

Drag scrolls the list with the same kinetic-decay feel as the
settings list. Long-press jumps to the active option (handy when the
list is long and the operator just wants to see the current pick).
"""

from __future__ import annotations

import asyncio
from collections.abc import Awaitable, Callable
from typing import ClassVar

from PIL import Image, ImageDraw

from ados.services.ui.dashboards.components import primitives as p
from ados.services.ui.pages.base import HitZone, PageContext
from ados.services.ui.pages.details._common import HEADER_H, draw_header_band
from ados.services.ui.touch.events import TouchGesture
from ados.services.ui.touch.kinetic import KineticDecay
from ados.services.ui.widgets.list_row import ROW_H

PAGE_W = 480
PAGE_H = 244

_BODY_Y = HEADER_H + 1
_BODY_H = PAGE_H - _BODY_Y


class EnumPickerModal:
    """Modal that surfaces a list of (value, label) choices."""

    id: ClassVar[str] = "modal.enum"
    refresh_hz: ClassVar[float] = 5.0

    def __init__(
        self,
        *,
        title: str,
        options: list[tuple[str, str]],
        current: str | None,
        on_save: Callable[[str], Awaitable[None]],
    ) -> None:
        self._title = title
        self._options = list(options)
        self._current = current
        self._on_save = on_save
        self._y_offset: int = 0
        self._kinetic = KineticDecay()
        self._move_task: asyncio.Task | None = None
        self._move_active: bool = False
        # Pre-seed the offset so the active option is visible.
        self._maybe_focus_current()

    def _maybe_focus_current(self) -> None:
        if self._current is None:
            return
        idx = self._index_of(self._current)
        if idx < 0:
            return
        target = idx * ROW_H
        # Only scroll down if the active row would be off-screen.
        if target + ROW_H > _BODY_H:
            self._y_offset = max(0, target - (_BODY_H - ROW_H))

    def _index_of(self, value: str) -> int:
        for i, (val, _) in enumerate(self._options):
            if val == value:
                return i
        return -1

    def _max_offset(self) -> int:
        total = ROW_H * len(self._options)
        return max(0, total - _BODY_H)

    async def on_enter(self, ctx: PageContext) -> None:
        ctx.logger.info(
            "modal_enum_enter",
            title=self._title,
            options=len(self._options),
            current=self._current,
        )
        if ctx.touch_move_bus is not None:
            self._move_active = True
            self._move_task = asyncio.create_task(self._consume_moves(ctx))

    async def on_leave(self, ctx: PageContext) -> None:
        ctx.logger.info("modal_enum_leave")
        self._move_active = False
        if self._move_task is not None and not self._move_task.done():
            self._move_task.cancel()
            try:
                await self._move_task
            except (asyncio.CancelledError, Exception):
                pass
        self._move_task = None

    async def _consume_moves(self, ctx: PageContext) -> None:
        bus = ctx.touch_move_bus
        if bus is None:
            return
        last_y: int | None = None
        try:
            async for move in bus.subscribe():
                if not self._move_active:
                    break
                if last_y is None:
                    last_y = move.y_lcd
                    continue
                dy = last_y - move.y_lcd
                last_y = move.y_lcd
                if dy:
                    self._scroll_by(dy)
        except asyncio.CancelledError:
            return
        except Exception as exc:  # noqa: BLE001
            ctx.logger.debug("modal_enum_move_loop_failed", error=str(exc))

    def _scroll_by(self, dy: int) -> None:
        max_off = self._max_offset()
        new_off = self._y_offset + dy
        # Allow 16 px overshoot at top/bottom for a rubber-band feel.
        new_off = max(-16, min(max_off + 16, new_off))
        self._y_offset = new_off

    async def render(self, ctx: PageContext) -> Image.Image:
        # Advance any kinetic decay before painting so the offset moves
        # smoothly even when on_touch isn't firing.
        if self._kinetic.active:
            offset_delta = self._kinetic.tick(1.0 / max(self.refresh_hz, 1.0))
            self._scroll_by(int(offset_delta))
        # Snap-back when overshot at rest.
        if not self._kinetic.active:
            max_off = self._max_offset()
            if self._y_offset < 0:
                self._y_offset = 0
            elif self._y_offset > max_off:
                self._y_offset = max_off

        palette = ctx.palette
        img = Image.new("RGB", (PAGE_W, PAGE_H), palette.bg_primary)
        draw_header_band(img, palette, self._title)
        d = ImageDraw.Draw(img)

        # Body strip: paint visible rows. Rows whose top would land
        # outside the visible band are skipped.
        body_top = _BODY_Y
        body_bottom = PAGE_H
        for i, (value, label) in enumerate(self._options):
            row_top_in_list = i * ROW_H - self._y_offset
            row_y = body_top + row_top_in_list
            if row_y + ROW_H <= body_top:
                continue
            if row_y >= body_bottom:
                break
            self._draw_option(
                img, d, palette, row_y, value, label,
                is_active=(value == self._current),
            )
        return img

    def _draw_option(
        self,
        img: Image.Image,
        d: ImageDraw.ImageDraw,
        palette,  # type: ignore[no-untyped-def]
        y: int,
        value: str,
        label: str,
        *,
        is_active: bool,
    ) -> None:
        if is_active:
            # Faint active fill in a tinted accent backdrop.
            d.rectangle(
                (0, max(_BODY_Y, y), PAGE_W - 1, min(PAGE_H - 1, y + ROW_H - 1)),
                fill=palette.bg_secondary,
            )
        label_font = p.font("sans_regular", 14)
        _, lh = p.text_size(img, label, label_font)
        d.text(
            (16, y + (ROW_H - lh) // 2 - 2),
            label,
            fill=palette.text_primary if is_active else palette.text_secondary,
            font=label_font,
        )
        if is_active:
            # Right-side checkmark in accent_primary.
            cx = PAGE_W - 24
            cy = y + ROW_H // 2
            d.line((cx - 8, cy, cx - 2, cy + 6), fill=palette.accent_primary, width=2)
            d.line((cx - 2, cy + 6, cx + 8, cy - 6), fill=palette.accent_primary, width=2)
        d.line(
            (0, y + ROW_H - 1, PAGE_W - 1, y + ROW_H - 1),
            fill=palette.border_default,
        )

    def hit_zones(self, ctx: PageContext) -> list[HitZone]:
        zones: list[HitZone] = [HitZone(id="details.back", x=8, y=8, w=40, h=32)]
        for i, (value, _) in enumerate(self._options):
            row_top_in_list = i * ROW_H - self._y_offset
            row_y = _BODY_Y + row_top_in_list
            if row_y + ROW_H <= _BODY_Y or row_y >= PAGE_H:
                continue
            zones.append(
                HitZone(
                    id=f"enum.option:{value}",
                    x=0,
                    y=row_y,
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
        if zone.id == "details.back" and gesture.kind == "tap":
            await ctx.navigator.pop_modal(ctx=ctx)
            return
        if gesture.kind == "drag":
            v = gesture.velocity_px_per_s
            if gesture.direction == "up":
                v = -v
            self._kinetic.start(v)
            return
        if gesture.kind == "long_press" and self._current is not None:
            idx = self._index_of(self._current)
            if idx >= 0:
                self._y_offset = idx * ROW_H
            return
        if gesture.kind != "tap":
            return
        if zone.id.startswith("enum.option:"):
            value = zone.id.removeprefix("enum.option:")
            self._current = value
            try:
                await self._on_save(value)
            except Exception as exc:  # noqa: BLE001
                ctx.logger.warning(
                    "modal_enum_save_failed",
                    value=value,
                    error=str(exc),
                )
                return
            await ctx.navigator.pop_modal(ctx=ctx)


def options_from_strings(values: list[str]) -> list[tuple[str, str]]:
    """Convenience helper: turn a flat list of values into ``(value, label)`` pairs."""
    return [(v, v) for v in values]
