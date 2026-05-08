"""Modal page that asks the operator to confirm a destructive action.

Used by reboot, factory reset, hotspot disable, and similar settings
rows. The dialog renders a header band, a multi-line body, and two
buttons at the bottom: Cancel (left, neutral) and Confirm (right,
accent or error). When ``confirm_destructive=True`` the confirm button
uses ``palette.status_error`` so a destructive operation reads in red.

Confirm fires ``on_confirm`` and pops the modal. Cancel pops without
firing.
"""

from __future__ import annotations

from collections.abc import Awaitable, Callable
from typing import ClassVar

from PIL import Image, ImageDraw

from ados.services.ui.dashboards.components import primitives as p
from ados.services.ui.pages.base import HitZone, PageContext
from ados.services.ui.pages.details._common import HEADER_H, draw_header_band
from ados.services.ui.touch.events import TouchGesture

PAGE_W = 480
PAGE_H = 244

_BODY_PAD = 16
_BTN_W = 200
_BTN_H = 44
_BTN_Y = PAGE_H - _BTN_H - 12


class ConfirmDialog:
    """Cancel + Confirm modal."""

    id: ClassVar[str] = "modal.confirm"
    refresh_hz: ClassVar[float] = 5.0

    def __init__(
        self,
        title: str,
        body: str,
        *,
        confirm_label: str = "Confirm",
        confirm_destructive: bool = False,
        on_confirm: Callable[[], Awaitable[None]],
    ) -> None:
        self._title = title
        self._body = body
        self._confirm_label = confirm_label
        self._destructive = confirm_destructive
        self._on_confirm = on_confirm

    async def on_enter(self, ctx: PageContext) -> None:
        ctx.logger.info(
            "modal_confirm_enter",
            title=self._title,
            destructive=self._destructive,
        )

    async def on_leave(self, ctx: PageContext) -> None:
        ctx.logger.info("modal_confirm_leave")

    async def render(self, ctx: PageContext) -> Image.Image:
        palette = ctx.palette
        img = Image.new("RGB", (PAGE_W, PAGE_H), palette.bg_primary)
        draw_header_band(img, palette, self._title)
        d = ImageDraw.Draw(img)

        body_font = p.font("sans_regular", 14)
        # Wrap-by-width: split on whitespace, accumulate up to about
        # 50 chars then break. Real wrap-by-pixel-width would be
        # nicer but the body strings are short prompts.
        lines = self._wrap(d, self._body, body_font, PAGE_W - 2 * _BODY_PAD)
        cy = HEADER_H + 16
        for line in lines:
            d.text((_BODY_PAD, cy), line, fill=palette.text_secondary, font=body_font)
            cy += 22

        # Cancel button (left).
        cancel_x = (PAGE_W // 2 - _BTN_W) // 2
        d.rectangle(
            (cancel_x, _BTN_Y, cancel_x + _BTN_W - 1, _BTN_Y + _BTN_H - 1),
            fill=palette.bg_secondary,
            outline=palette.border_strong,
            width=1,
        )
        cf = p.font("sans_bold", 14)
        cw, ch = p.text_size(img, "Cancel", cf)
        d.text(
            (cancel_x + (_BTN_W - cw) // 2, _BTN_Y + (_BTN_H - ch) // 2 - 1),
            "Cancel",
            fill=palette.text_primary,
            font=cf,
        )

        # Confirm button (right).
        confirm_x = PAGE_W // 2 + (PAGE_W // 2 - _BTN_W) // 2
        confirm_color = (
            palette.status_error if self._destructive else palette.accent_primary
        )
        d.rectangle(
            (confirm_x, _BTN_Y, confirm_x + _BTN_W - 1, _BTN_Y + _BTN_H - 1),
            fill=confirm_color,
        )
        cw, ch = p.text_size(img, self._confirm_label, cf)
        d.text(
            (confirm_x + (_BTN_W - cw) // 2, _BTN_Y + (_BTN_H - ch) // 2 - 1),
            self._confirm_label,
            fill=palette.text_primary,
            font=cf,
        )
        return img

    def _wrap(
        self,
        d: ImageDraw.ImageDraw,
        text: str,
        font,  # type: ignore[no-untyped-def]
        max_width: int,
    ) -> list[str]:
        words = text.split()
        if not words:
            return [""]
        lines: list[str] = []
        current = ""
        for word in words:
            candidate = (current + " " + word).strip()
            cw, _ = p.text_size(d, candidate, font)
            if cw <= max_width or not current:
                current = candidate
            else:
                lines.append(current)
                current = word
        if current:
            lines.append(current)
        return lines

    def hit_zones(self, ctx: PageContext) -> list[HitZone]:
        cancel_x = (PAGE_W // 2 - _BTN_W) // 2
        confirm_x = PAGE_W // 2 + (PAGE_W // 2 - _BTN_W) // 2
        return [
            HitZone(id="details.back", x=8, y=8, w=40, h=32),
            HitZone(
                id="confirm.cancel",
                x=cancel_x,
                y=_BTN_Y,
                w=_BTN_W,
                h=_BTN_H,
            ),
            HitZone(
                id="confirm.ok",
                x=confirm_x,
                y=_BTN_Y,
                w=_BTN_W,
                h=_BTN_H,
            ),
        ]

    async def on_touch(
        self,
        ctx: PageContext,
        zone: HitZone,
        gesture: TouchGesture,
    ) -> None:
        if gesture.kind != "tap":
            return
        if zone.id in ("details.back", "confirm.cancel"):
            await ctx.navigator.pop_modal(ctx=ctx)
            return
        if zone.id == "confirm.ok":
            try:
                await self._on_confirm()
            except Exception as exc:  # noqa: BLE001
                ctx.logger.warning("modal_confirm_action_failed", error=str(exc))
            await ctx.navigator.pop_modal(ctx=ctx)
