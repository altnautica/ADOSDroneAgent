"""Modal page that edits an integer value via a track + stepper buttons.

Used by any settings row whose underlying value is a numeric
parameter inside a fixed envelope: TX power, brightness, telemetry
rate. The picker draws a big numeric readout, a thick draggable
track, two large step buttons, and a Save button at the bottom.

Save fires the supplied ``on_save`` coroutine with the chosen value
and pops the modal. Cancel (back chevron) pops without saving.
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

PAGE_W = 480
PAGE_H = 244

# Big readout band.
_READOUT_Y = HEADER_H + 12
_READOUT_H = 56

# Stepper buttons + slider track.
_STEP_W = 60
_STEP_H = 60
_STEP_PAD = 16
_TRACK_X = _STEP_PAD + _STEP_W + 12
_TRACK_W = PAGE_W - 2 * (_STEP_PAD + _STEP_W + 12)
_TRACK_Y = _READOUT_Y + _READOUT_H + 18
_TRACK_THICK = 12
_THUMB_W = 26
_THUMB_H = 36

# Save button band.
_SAVE_W = 200
_SAVE_H = 36
_SAVE_Y = PAGE_H - _SAVE_H - 8


class SliderModal:
    """Numeric slider modal with on-screen save."""

    id: ClassVar[str] = "modal.slider"
    refresh_hz: ClassVar[float] = 5.0

    def __init__(
        self,
        *,
        title: str,
        min_val: int,
        max_val: int,
        step: int,
        current: int,
        unit: str,
        on_save: Callable[[int], Awaitable[None]],
    ) -> None:
        if min_val >= max_val:
            raise ValueError("min_val must be < max_val")
        self._title = title
        self._min = int(min_val)
        self._max = int(max_val)
        self._step = max(1, int(step))
        self._unit = unit
        self._on_save = on_save
        self._value = max(self._min, min(self._max, int(current)))
        self._dragging: bool = False
        self._move_task: asyncio.Task | None = None
        self._move_active: bool = False

    def _clamp(self, v: int) -> int:
        return max(self._min, min(self._max, int(v)))

    def _value_for_x(self, x: int) -> int:
        rel = max(0, min(_TRACK_W - 1, x - _TRACK_X))
        frac = rel / max(1, _TRACK_W - 1)
        raw = self._min + frac * (self._max - self._min)
        # Snap to step.
        steps = round((raw - self._min) / self._step)
        return self._clamp(self._min + steps * self._step)

    async def on_enter(self, ctx: PageContext) -> None:
        ctx.logger.info(
            "modal_slider_enter",
            title=self._title,
            current=self._value,
            min=self._min,
            max=self._max,
        )
        if ctx.touch_move_bus is not None:
            self._move_active = True
            self._move_task = asyncio.create_task(self._consume_moves(ctx))

    async def on_leave(self, ctx: PageContext) -> None:
        ctx.logger.info("modal_slider_leave")
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
        try:
            async for move in bus.subscribe():
                if not self._move_active:
                    break
                if not self._dragging:
                    continue
                if _TRACK_Y - 16 <= move.y_lcd <= _TRACK_Y + _THUMB_H + 16:
                    self._value = self._value_for_x(move.x_lcd)
        except asyncio.CancelledError:
            return
        except Exception as exc:  # noqa: BLE001
            ctx.logger.debug("modal_slider_move_loop_failed", error=str(exc))

    async def render(self, ctx: PageContext) -> Image.Image:
        palette = ctx.palette
        img = Image.new("RGB", (PAGE_W, PAGE_H), palette.bg_primary)
        draw_header_band(img, palette, self._title)
        d = ImageDraw.Draw(img)

        # Big numeric readout, centered.
        big_font = p.font("sans_bold", 28)
        text = f"{self._value} {self._unit}".strip()
        tw, th = p.text_size(img, text, big_font)
        d.text(
            ((PAGE_W - tw) // 2, _READOUT_Y + (_READOUT_H - th) // 2),
            text,
            fill=palette.text_primary,
            font=big_font,
        )
        # Min/max captions under the readout.
        cap_font = p.font("mono_regular", 11)
        cap_text = f"{self._min} … {self._max}"
        cw, ch = p.text_size(img, cap_text, cap_font)
        d.text(
            ((PAGE_W - cw) // 2, _READOUT_Y + _READOUT_H - 2),
            cap_text,
            fill=palette.text_tertiary,
            font=cap_font,
        )

        # − stepper.
        self._draw_step_button(d, palette, _STEP_PAD, _TRACK_Y - (_STEP_H - _THUMB_H) // 2, "−")
        # + stepper.
        self._draw_step_button(
            d,
            palette,
            PAGE_W - _STEP_PAD - _STEP_W,
            _TRACK_Y - (_STEP_H - _THUMB_H) // 2,
            "+",
        )

        # Track.
        track_y0 = _TRACK_Y + (_THUMB_H - _TRACK_THICK) // 2
        d.rectangle(
            (
                _TRACK_X,
                track_y0,
                _TRACK_X + _TRACK_W - 1,
                track_y0 + _TRACK_THICK - 1,
            ),
            fill=palette.bg_tertiary,
            outline=palette.border_default,
            width=1,
        )
        # Filled portion up to thumb.
        frac = (self._value - self._min) / max(1, (self._max - self._min))
        thumb_cx = _TRACK_X + int(round(frac * (_TRACK_W - 1)))
        d.rectangle(
            (_TRACK_X, track_y0, thumb_cx, track_y0 + _TRACK_THICK - 1),
            fill=palette.accent_primary,
        )
        # Thumb.
        thumb_x0 = thumb_cx - _THUMB_W // 2
        d.rectangle(
            (thumb_x0, _TRACK_Y, thumb_x0 + _THUMB_W - 1, _TRACK_Y + _THUMB_H - 1),
            fill=palette.accent_primary,
            outline=palette.text_primary,
            width=1,
        )

        # Save button.
        save_x = (PAGE_W - _SAVE_W) // 2
        d.rectangle(
            (save_x, _SAVE_Y, save_x + _SAVE_W - 1, _SAVE_Y + _SAVE_H - 1),
            fill=palette.accent_primary,
        )
        save_font = p.font("sans_bold", 14)
        sw, sh = p.text_size(img, "Save", save_font)
        d.text(
            (save_x + (_SAVE_W - sw) // 2, _SAVE_Y + (_SAVE_H - sh) // 2 - 1),
            "Save",
            fill=palette.text_primary,
            font=save_font,
        )
        return img

    def _draw_step_button(
        self,
        d: ImageDraw.ImageDraw,
        palette,  # type: ignore[no-untyped-def]
        x: int,
        y: int,
        glyph: str,
    ) -> None:
        d.rectangle(
            (x, y, x + _STEP_W - 1, y + _STEP_H - 1),
            fill=palette.bg_secondary,
            outline=palette.border_strong,
            width=1,
        )
        cx = x + _STEP_W // 2
        cy = y + _STEP_H // 2
        if glyph == "−":
            d.line((cx - 14, cy, cx + 14, cy), fill=palette.text_primary, width=3)
        else:
            d.line((cx - 14, cy, cx + 14, cy), fill=palette.text_primary, width=3)
            d.line((cx, cy - 14, cx, cy + 14), fill=palette.text_primary, width=3)

    def hit_zones(self, ctx: PageContext) -> list[HitZone]:
        save_x = (PAGE_W - _SAVE_W) // 2
        return [
            HitZone(id="details.back", x=8, y=8, w=40, h=32),
            HitZone(
                id="slider.minus",
                x=_STEP_PAD,
                y=_TRACK_Y - (_STEP_H - _THUMB_H) // 2,
                w=_STEP_W,
                h=_STEP_H,
            ),
            HitZone(
                id="slider.plus",
                x=PAGE_W - _STEP_PAD - _STEP_W,
                y=_TRACK_Y - (_STEP_H - _THUMB_H) // 2,
                w=_STEP_W,
                h=_STEP_H,
            ),
            HitZone(
                id="slider.track",
                x=_TRACK_X - 8,
                y=_TRACK_Y - 8,
                w=_TRACK_W + 16,
                h=_THUMB_H + 16,
            ),
            HitZone(
                id="slider.save",
                x=save_x,
                y=_SAVE_Y,
                w=_SAVE_W,
                h=_SAVE_H,
            ),
        ]

    async def on_touch(
        self,
        ctx: PageContext,
        zone: HitZone,
        gesture: TouchGesture,
    ) -> None:
        if zone.id == "details.back" and gesture.kind == "tap":
            await ctx.navigator.pop_modal(ctx=ctx)
            return
        if zone.id == "slider.minus" and gesture.kind == "tap":
            self._value = self._clamp(self._value - self._step)
            return
        if zone.id == "slider.plus" and gesture.kind == "tap":
            self._value = self._clamp(self._value + self._step)
            return
        if zone.id == "slider.track":
            if gesture.kind == "tap":
                self._value = self._value_for_x(gesture.start_x)
                return
            if gesture.kind == "drag":
                self._dragging = False
                self._value = self._value_for_x(gesture.end_x)
                return
            # Drag-start arrives implicitly via the move-bus loop on
            # the very first sample; we flip the flag here so the
            # consumer starts honoring move events.
            self._dragging = True
            return
        if zone.id == "slider.save" and gesture.kind == "tap":
            try:
                await self._on_save(self._value)
            except Exception as exc:  # noqa: BLE001
                ctx.logger.warning(
                    "modal_slider_save_failed",
                    value=self._value,
                    error=str(exc),
                )
                return
            await ctx.navigator.pop_modal(ctx=ctx)
