"""Modal page that captures a string via an on-screen keyboard.

Used by the settings rows whose values are text — Wi-Fi SSID, Wi-Fi
password, hotspot SSID, hotspot password. Layout follows the approved
mockup: a 32 px input strip at the top of the body and four rows of
keys at 40x44 each. Shift toggles between letters / symbols. Save
fires the supplied ``on_save`` coroutine and pops the modal.

When ``masked=True`` the rendered input strip shows ``•`` per
character so a password is not echoed in plaintext.
"""

from __future__ import annotations

import time
from collections.abc import Awaitable, Callable
from typing import ClassVar

from PIL import Image, ImageDraw

from ados.services.ui.dashboards.components import primitives as p
from ados.services.ui.pages.base import HitZone, PageContext
from ados.services.ui.pages.details._common import HEADER_H, draw_header_band
from ados.services.ui.touch.events import TouchGesture

PAGE_W = 480
PAGE_H = 244

_INPUT_Y = HEADER_H + 4
_INPUT_H = 32
_KEY_W = 40
_KEY_H = 44
_KEY_GAP = 0
_ROWS_Y = _INPUT_Y + _INPUT_H + 4

# 12 columns x 4 rows. The leftover horizontal pixel (480 - 12 * 40)
# = 0 — exact fit. Keys are flush to the page edges.
_KEYS_PER_ROW = 12
_TOTAL_KB_W = _KEYS_PER_ROW * _KEY_W

_LETTERS_LOWER = [
    list("qwertyuiop[]"),
    list("asdfghjkl;'`"),
    list("zxcvbnm,./-="),
    ["SHIFT", "123", "SPACE", "BKSP", "SAVE"],
]
_LETTERS_UPPER = [[c.upper() for c in row] for row in _LETTERS_LOWER[:3]] + [
    ["SHIFT", "123", "SPACE", "BKSP", "SAVE"]
]
_DIGITS = [
    list("1234567890-="),
    list("!@#$%^&*()_+"),
    list("{}[]<>:;\"',."),
    ["SHIFT", "ABC", "SPACE", "BKSP", "SAVE"],
]

# Bottom row spans the 12 columns with widths set per key. Sums to 12.
_BOTTOM_LAYOUT = [("SHIFT", 2), ("123", 2), ("SPACE", 4), ("BKSP", 2), ("SAVE", 2)]
# The "ABC" variant just renames the 123 key when in digits mode.
_BOTTOM_LAYOUT_DIGITS = [("SHIFT", 2), ("ABC", 2), ("SPACE", 4), ("BKSP", 2), ("SAVE", 2)]

_LONG_PRESS_BACKSPACE_MS = 400


class KeyboardModal:
    """On-screen keyboard for short text fields."""

    id: ClassVar[str] = "modal.keyboard"
    refresh_hz: ClassVar[float] = 5.0

    def __init__(
        self,
        *,
        title: str,
        initial: str = "",
        placeholder: str = "",
        masked: bool = False,
        on_save: Callable[[str], Awaitable[None]],
    ) -> None:
        self._title = title
        self._value = str(initial)
        self._placeholder = placeholder
        self._masked = masked
        self._on_save = on_save
        # Modes: "letters_lower" / "letters_upper" / "digits".
        self._mode: str = "letters_lower"
        self._last_backspace_ms: int = 0

    async def on_enter(self, ctx: PageContext) -> None:
        ctx.logger.info(
            "modal_keyboard_enter",
            title=self._title,
            initial_len=len(self._value),
            masked=self._masked,
        )

    async def on_leave(self, ctx: PageContext) -> None:
        ctx.logger.info("modal_keyboard_leave")

    def _rows_for_mode(self) -> list[list[str]]:
        if self._mode == "letters_upper":
            return _LETTERS_UPPER
        if self._mode == "digits":
            return _DIGITS
        return _LETTERS_LOWER

    def _bottom_layout(self) -> list[tuple[str, int]]:
        return _BOTTOM_LAYOUT_DIGITS if self._mode == "digits" else _BOTTOM_LAYOUT

    async def render(self, ctx: PageContext) -> Image.Image:
        palette = ctx.palette
        img = Image.new("RGB", (PAGE_W, PAGE_H), palette.bg_primary)
        draw_header_band(img, palette, self._title)
        d = ImageDraw.Draw(img)

        # Input strip.
        d.rectangle(
            (8, _INPUT_Y, PAGE_W - 9, _INPUT_Y + _INPUT_H - 1),
            fill=palette.bg_secondary,
            outline=palette.border_strong,
            width=1,
        )
        text = (
            ("•" * len(self._value))
            if (self._masked and self._value)
            else self._value
        )
        font = p.font("mono_regular", 14)
        if not text and self._placeholder:
            d.text(
                (16, _INPUT_Y + 7),
                self._placeholder,
                fill=palette.text_tertiary,
                font=font,
            )
        else:
            d.text(
                (16, _INPUT_Y + 7),
                text or "",
                fill=palette.text_primary,
                font=font,
            )
        # Caret at end of text.
        if not self._placeholder or text:
            tw, _ = p.text_size(img, text or "", font)
            cx = 16 + tw + 1
            if cx < PAGE_W - 16:
                d.line(
                    (cx, _INPUT_Y + 6, cx, _INPUT_Y + _INPUT_H - 6),
                    fill=palette.text_primary,
                    width=1,
                )

        # Top three rows: 12 keys each.
        rows = self._rows_for_mode()
        for r in range(3):
            row = rows[r]
            for c, label in enumerate(row[:_KEYS_PER_ROW]):
                self._draw_key(d, palette, _KEY_W * c, _ROWS_Y + r * _KEY_H, _KEY_W, _KEY_H, label)

        # Bottom row, variable widths.
        x_cursor = 0
        bottom = self._bottom_layout()
        for label, span in bottom:
            w = _KEY_W * span
            self._draw_key(
                d,
                palette,
                x_cursor,
                _ROWS_Y + 3 * _KEY_H,
                w,
                _KEY_H,
                label,
                primary=(label == "SAVE"),
            )
            x_cursor += w
        return img

    def _draw_key(
        self,
        d: ImageDraw.ImageDraw,
        palette,  # type: ignore[no-untyped-def]
        x: int,
        y: int,
        w: int,
        h: int,
        label: str,
        primary: bool = False,
    ) -> None:
        bg = palette.accent_primary if primary else palette.bg_secondary
        border = palette.accent_primary if primary else palette.border_default
        d.rectangle((x, y, x + w - 1, y + h - 1), fill=bg, outline=border, width=1)
        font = p.font("sans_bold", 14)
        # Wider labels (SHIFT, BKSP, SAVE etc.) get smaller font for fit.
        if label in ("SHIFT", "BKSP", "SAVE", "SPACE", "123", "ABC"):
            font = p.font("sans_bold", 11)
        # Visual: SHIFT pressed when in upper mode.
        text_color = palette.text_primary
        if label == "SHIFT" and self._mode == "letters_upper":
            text_color = palette.accent_primary
        if label == "SPACE":
            display = "space"
        elif label == "BKSP":
            display = "←"
            font = p.font("sans_bold", 18)
        else:
            display = label
        tw, th = p.text_size(d, display, font)
        d.text(
            (x + (w - tw) // 2, y + (h - th) // 2 - 1),
            display,
            fill=text_color,
            font=font,
        )

    def hit_zones(self, ctx: PageContext) -> list[HitZone]:
        zones: list[HitZone] = [HitZone(id="details.back", x=8, y=8, w=40, h=32)]
        rows = self._rows_for_mode()
        for r in range(3):
            row = rows[r]
            for c, label in enumerate(row[:_KEYS_PER_ROW]):
                zones.append(
                    HitZone(
                        id=f"kb.key:{r}:{c}:{label}",
                        x=_KEY_W * c,
                        y=_ROWS_Y + r * _KEY_H,
                        w=_KEY_W,
                        h=_KEY_H,
                    )
                )
        x_cursor = 0
        for label, span in self._bottom_layout():
            w = _KEY_W * span
            zones.append(
                HitZone(
                    id=f"kb.fn:{label}",
                    x=x_cursor,
                    y=_ROWS_Y + 3 * _KEY_H,
                    w=w,
                    h=_KEY_H,
                )
            )
            x_cursor += w
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
        if zone.id.startswith("kb.key:") and gesture.kind == "tap":
            # zone id format: "kb.key:<row>:<col>:<label>"
            parts = zone.id.split(":", 3)
            if len(parts) == 4:
                self._value += parts[3]
            return
        if zone.id.startswith("kb.fn:"):
            label = zone.id.removeprefix("kb.fn:")
            await self._handle_fn_key(ctx, label, gesture)

    async def _handle_fn_key(
        self,
        ctx: PageContext,
        label: str,
        gesture: TouchGesture,
    ) -> None:
        now_ms = int(time.monotonic() * 1000)
        if label == "SHIFT" and gesture.kind == "tap":
            self._mode = (
                "letters_upper" if self._mode == "letters_lower" else "letters_lower"
            )
            return
        if label == "123" and gesture.kind == "tap":
            self._mode = "digits"
            return
        if label == "ABC" and gesture.kind == "tap":
            self._mode = "letters_lower"
            return
        if label == "SPACE" and gesture.kind == "tap":
            self._value += " "
            return
        if label == "BKSP":
            if gesture.kind == "long_press":
                self._value = ""
                return
            if gesture.kind == "tap":
                self._last_backspace_ms = now_ms
                self._value = self._value[:-1]
                return
        if label == "SAVE" and gesture.kind == "tap":
            try:
                await self._on_save(self._value)
            except Exception as exc:  # noqa: BLE001
                ctx.logger.warning("modal_keyboard_save_failed", error=str(exc))
                return
            await ctx.navigator.pop_modal(ctx=ctx)
