"""Pair-drone detail page reachable from the More tab.

This page mirrors the dashboard's DRONE-tile drilldown for the WFB
radio pairing surface. It has two render paths:

* **Paired** — when ``state.paired_drone.device_id`` is truthy. Show
  the device id, key fingerprint short form, paired-at relative + a
  short absolute timestamp, and an "Unpair" destructive button at the
  bottom-right that fires
  ``POST /api/wfb/pair/unpair`` after a confirm dialog.

* **Unpaired** — when no device is paired. Show the local pairing
  code from ``state.pairing.code`` (falls back to
  ``state.cloud.pairing_code``), a 100x100 QR code that encodes the
  same code, and an "Open pairing window" accent button that fires
  ``POST /api/v1/pair/local-bind``. While a pairing window is active
  the button is replaced by a countdown showing the remaining time.

Hit zones:

* ``details.back`` — back chevron in the header.
* ``pair.unpair`` — destructive button (paired view only).
* ``pair.open_window`` — accent button (unpaired view only, hidden
  while a window is active).
"""

from __future__ import annotations

from typing import Any, ClassVar

from PIL import Image, ImageDraw

from ados.services.ui.dashboards.components import primitives as p
from ados.services.ui.dashboards.components.qr import render_qr
from ados.services.ui.touch.events import TouchGesture
from ados.services.ui.widgets import ConfirmDialog

from ..base import HitZone, PageContext
from ._common import HEADER_H, draw_header_band

PAGE_W = 480
PAGE_H = 244

# Action button geometry — bottom-right corner, 60 px tall (matches
# the brief), wide enough to hold a 5-letter label comfortably at
# DejaVu Sans Bold 14.
_BTN_W = 180
_BTN_H = 40
_BTN_RIGHT_PAD = 12
_BTN_BOTTOM_PAD = 12


def _safe_dict(value: Any) -> dict:
    return value if isinstance(value, dict) else {}


def _format_relative(seconds: float | None) -> str:
    if seconds is None or seconds < 0:
        return "--"
    if seconds < 60:
        return f"{int(seconds)}s ago"
    if seconds < 3600:
        return f"{int(seconds // 60)}m ago"
    if seconds < 86400:
        return f"{int(seconds // 3600)}h ago"
    return f"{int(seconds // 86400)}d ago"


def _format_short_clock(timestamp: float | None) -> str:
    """Return ``HH:MM:SS`` for a unix timestamp, or ``--`` on miss."""
    if not isinstance(timestamp, (int, float)) or timestamp <= 0:
        return "--"
    try:
        import time as _time

        lt = _time.localtime(timestamp)
        return f"{lt.tm_hour:02d}:{lt.tm_min:02d}:{lt.tm_sec:02d}"
    except (ValueError, OSError):
        return "--"


class PairDroneDetailPage:
    """Drilldown — manage the WFB radio pair from the LCD."""

    id: ClassVar[str] = "details.pair_drone"
    refresh_hz: ClassVar[float] = 2.0

    def __init__(self) -> None:
        self._wfb_pair: dict[str, Any] = {}

    async def on_enter(self, ctx: PageContext) -> None:
        ctx.logger.info("details_pair_drone_enter")
        await self._refresh(ctx)

    async def on_leave(self, ctx: PageContext) -> None:
        ctx.logger.info("details_pair_drone_leave")

    async def _refresh(self, ctx: PageContext) -> None:
        client = ctx.http
        if client is None:
            return
        try:
            r = await client.get("/api/wfb/pair", timeout=1.5)
            if r.status_code == 200:
                blob = r.json() if callable(getattr(r, "json", None)) else {}
                if isinstance(blob, dict):
                    self._wfb_pair = blob
        except Exception as exc:  # noqa: BLE001
            ctx.logger.debug(
                "details_pair_drone_status_fetch_failed",
                error=str(exc),
            )

    # ── render ─────────────────────────────────────────────────

    async def render(self, ctx: PageContext) -> Image.Image:
        await self._refresh(ctx)
        palette = ctx.palette
        img = Image.new("RGB", (PAGE_W, PAGE_H), palette.bg_primary)
        draw_header_band(img, palette, "Pair drone")
        d = ImageDraw.Draw(img)

        paired = _safe_dict(ctx.state.get("paired_drone"))
        device_id = paired.get("device_id") or self._wfb_pair.get("peer_device_id")

        if device_id:
            self._render_paired(img, d, palette, paired, device_id, ctx)
        else:
            self._render_unpaired(img, d, palette, ctx)
        return img

    def _render_paired(
        self,
        img: Image.Image,
        d: ImageDraw.ImageDraw,
        palette,  # type: ignore[no-untyped-def]
        paired: dict,
        device_id: str,
        ctx: PageContext,
    ) -> None:
        mono = p.font("mono_regular", 12)
        label = p.font("sans_bold", 11)
        # Identity rows.
        cy = HEADER_H + 12
        d.text(
            (16, cy),
            "DEVICE ID",
            fill=palette.text_tertiary,
            font=label,
        )
        d.text(
            (16, cy + 14),
            str(device_id),
            fill=palette.text_primary,
            font=mono,
        )
        cy += 36

        fingerprint = (
            paired.get("key_fingerprint")
            or self._wfb_pair.get("peer_fingerprint")
            or self._wfb_pair.get("key_fingerprint")
            or ""
        )
        if fingerprint:
            short = (
                f"{fingerprint[:12]}…{fingerprint[-4:]}"
                if len(fingerprint) > 20
                else str(fingerprint)
            )
        else:
            short = "--"
        d.text(
            (16, cy),
            "KEY",
            fill=palette.text_tertiary,
            font=label,
        )
        d.text(
            (16, cy + 14),
            short,
            fill=palette.text_secondary,
            font=mono,
        )
        cy += 36

        paired_at_seconds = paired.get("paired_at_seconds")
        paired_at_ts = paired.get("paired_at") or self._wfb_pair.get("paired_at")
        rel = _format_relative(
            paired_at_seconds if isinstance(paired_at_seconds, (int, float)) else None
        )
        absolute = _format_short_clock(
            paired_at_ts if isinstance(paired_at_ts, (int, float)) else None
        )
        d.text(
            (16, cy),
            "PAIRED",
            fill=palette.text_tertiary,
            font=label,
        )
        d.text(
            (16, cy + 14),
            f"{rel}  ({absolute})",
            fill=palette.text_secondary,
            font=mono,
        )

        # Unpair button — bottom-right.
        btn_x = PAGE_W - _BTN_W - _BTN_RIGHT_PAD
        btn_y = PAGE_H - _BTN_H - _BTN_BOTTOM_PAD
        d.rectangle(
            (btn_x, btn_y, btn_x + _BTN_W - 1, btn_y + _BTN_H - 1),
            fill=palette.status_error,
        )
        btn_label = "Unpair"
        bf = p.font("sans_bold", 14)
        bw, bh = p.text_size(img, btn_label, bf)
        d.text(
            (btn_x + (_BTN_W - bw) // 2, btn_y + (_BTN_H - bh) // 2 - 1),
            btn_label,
            fill=palette.text_primary,
            font=bf,
        )

    def _render_unpaired(
        self,
        img: Image.Image,
        d: ImageDraw.ImageDraw,
        palette,  # type: ignore[no-untyped-def]
        ctx: PageContext,
    ) -> None:
        cloud = _safe_dict(ctx.state.get("cloud"))
        pairing = _safe_dict(ctx.state.get("pairing"))
        code = (
            pairing.get("code")
            or cloud.get("pairing_code")
            or cloud.get("pair_code")
            or ""
        )

        msg_font = p.font("sans_bold", 13)
        msg = "NOT PAIRED"
        d.text(
            (16, HEADER_H + 8),
            msg,
            fill=palette.text_secondary,
            font=msg_font,
        )

        code_font = p.font("mono_bold", 22)
        code_text = str(code) if code else "------"
        d.text(
            (16, HEADER_H + 28),
            code_text,
            fill=palette.text_primary,
            font=code_font,
        )

        # 100x100 QR centered on the right half.
        if code:
            qr_payload = (
                cloud.get("pair_url")
                or pairing.get("pair_url")
                or f"altnautica.com/command?pair={code}"
            )
            qr = render_qr(str(qr_payload), target_px=100)
            if qr is not None:
                qr_x = PAGE_W - 100 - 24
                qr_y = HEADER_H + 8
                img.paste(qr, (qr_x, qr_y))

        # Pairing window state — the agent surfaces this as
        # ``state.pairing.window`` once the operator hits Open. The
        # WFB pair manager exposes the same shape on the REST snapshot
        # (``self._wfb_pair["window"]``) so we union both sources.
        window = _safe_dict(pairing.get("window")) or _safe_dict(
            self._wfb_pair.get("window")
        )
        window_active = bool(window.get("active") or window.get("open"))
        window_remaining = window.get("remaining_seconds")

        btn_x = PAGE_W - _BTN_W - _BTN_RIGHT_PAD
        btn_y = PAGE_H - _BTN_H - _BTN_BOTTOM_PAD

        if window_active:
            # Render a status pill where the button would normally live.
            secs = (
                int(window_remaining)
                if isinstance(window_remaining, (int, float))
                else 0
            )
            mins, rem = divmod(max(0, secs), 60)
            countdown = f"Open · {mins}:{rem:02d} left"
            d.rectangle(
                (btn_x, btn_y, btn_x + _BTN_W - 1, btn_y + _BTN_H - 1),
                fill=palette.bg_secondary,
                outline=palette.accent_primary,
                width=2,
            )
            cf = p.font("sans_bold", 13)
            cw, ch = p.text_size(img, countdown, cf)
            d.text(
                (btn_x + (_BTN_W - cw) // 2, btn_y + (_BTN_H - ch) // 2 - 1),
                countdown,
                fill=palette.accent_primary,
                font=cf,
            )
            return

        # Idle — paint the call-to-action button.
        d.rectangle(
            (btn_x, btn_y, btn_x + _BTN_W - 1, btn_y + _BTN_H - 1),
            fill=palette.accent_primary,
        )
        btn_label = "Open pairing"
        bf = p.font("sans_bold", 14)
        bw, bh = p.text_size(img, btn_label, bf)
        d.text(
            (btn_x + (_BTN_W - bw) // 2, btn_y + (_BTN_H - bh) // 2 - 1),
            btn_label,
            fill=palette.text_primary,
            font=bf,
        )

    # ── hit zones + dispatch ───────────────────────────────────

    def hit_zones(self, ctx: PageContext) -> list[HitZone]:
        zones: list[HitZone] = [HitZone(id="details.back", x=8, y=8, w=40, h=32)]
        paired = _safe_dict(ctx.state.get("paired_drone"))
        device_id = paired.get("device_id") or self._wfb_pair.get("peer_device_id")
        btn_x = PAGE_W - _BTN_W - _BTN_RIGHT_PAD
        btn_y = PAGE_H - _BTN_H - _BTN_BOTTOM_PAD
        if device_id:
            zones.append(
                HitZone(
                    id="pair.unpair",
                    x=btn_x,
                    y=btn_y,
                    w=_BTN_W,
                    h=_BTN_H,
                )
            )
        else:
            pairing = _safe_dict(ctx.state.get("pairing"))
            window = _safe_dict(pairing.get("window")) or _safe_dict(
                self._wfb_pair.get("window")
            )
            if not (window.get("active") or window.get("open")):
                zones.append(
                    HitZone(
                        id="pair.open_window",
                        x=btn_x,
                        y=btn_y,
                        w=_BTN_W,
                        h=_BTN_H,
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
        if zone.id == "details.back":
            await ctx.navigator.pop_modal(ctx=ctx)
            return
        if zone.id == "pair.unpair":
            await self._show_unpair_dialog(ctx)
            return
        if zone.id == "pair.open_window":
            await self._open_pairing_window(ctx)
            return

    async def _show_unpair_dialog(self, ctx: PageContext) -> None:
        async def _on_confirm() -> None:
            client = ctx.http
            if client is None:
                return
            try:
                await client.post("/api/wfb/pair/unpair", timeout=2.0)
                ctx.logger.info("details_pair_drone_unpair_dispatched")
            except Exception as exc:  # noqa: BLE001
                ctx.logger.warning(
                    "details_pair_drone_unpair_failed", error=str(exc),
                )
            await self._refresh(ctx)

        await ctx.navigator.push_modal(
            ConfirmDialog(
                "Unpair drone",
                (
                    "Erases the radio pair keys and stops wfb-rx. The drone "
                    "must be re-paired before video and telemetry resume."
                ),
                confirm_label="Unpair",
                confirm_destructive=True,
                on_confirm=_on_confirm,
            ),
            ctx=ctx,
        )

    async def _open_pairing_window(self, ctx: PageContext) -> None:
        client = ctx.http
        if client is None:
            return
        try:
            await client.post(
                "/api/v1/pair/local-bind",
                json={},
                timeout=2.0,
            )
            ctx.logger.info("details_pair_drone_open_window_dispatched")
        except Exception as exc:  # noqa: BLE001
            ctx.logger.debug(
                "details_pair_drone_open_window_failed", error=str(exc),
            )
        await self._refresh(ctx)
