"""Drone detail page.

Drilldown opened from the dashboard's DRONE tile. Two render paths:

* **Paired** (``state.paired_drone.device_id`` is truthy) — show
  device id, key fingerprint, paired-at relative time, and a 2-col
  grid covering vehicle / mode / armed / battery / GPS on the left
  and a battery graphic + 60 s sparkline on the right.

* **Unpaired** — show the pairing code as text plus a 100x100 QR code
  and an "Open pairing window" button that fires
  ``POST /api/v1/pair/local-bind``.

Live FC telemetry comes from ``GET /api/v1/dashboard/snapshot`` ``fc``
block; the paired-drone block itself is sourced from
``state.paired_drone`` which is already populated from the
``/api/v1/ground-station/status`` poll the OLED service runs at 1 Hz.
"""

from __future__ import annotations

import asyncio
from collections import deque
from typing import Any, ClassVar

from PIL import Image, ImageDraw

from ados.services.ui.dashboards.components import primitives as p
from ados.services.ui.dashboards.components.qr import render_qr
from ados.services.ui.dashboards.components.sparkline import draw_sparkline
from ados.services.ui.touch.events import TouchGesture

from ..base import HitZone, PageContext
from ._common import HEADER_H, draw_header_band

PAGE_W = 480
PAGE_H = 244

# Battery sparkline ring buffer kept on the page instance — 60 samples
# covers a minute of 1 Hz polling.
_BATTERY_HISTORY_MAX = 60


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


class DroneDetailPage:
    """Detail view for the DRONE dashboard tile."""

    id: ClassVar[str] = "details.drone"
    refresh_hz: ClassVar[float] = 2.0

    def __init__(self) -> None:
        self._fc: dict[str, Any] = {}
        self._paired_at_ms: int | None = None
        self._battery_history: deque[float | None] = deque(maxlen=_BATTERY_HISTORY_MAX)
        self._fetch_task: asyncio.Task | None = None

    async def on_enter(self, ctx: PageContext) -> None:
        ctx.logger.info("details_drone_enter")
        await self._refresh(ctx)

    async def on_leave(self, ctx: PageContext) -> None:
        ctx.logger.info("details_drone_leave")
        if self._fetch_task is not None and not self._fetch_task.done():
            self._fetch_task.cancel()
            try:
                await self._fetch_task
            except (asyncio.CancelledError, Exception):
                pass
        self._fetch_task = None

    async def _refresh(self, ctx: PageContext) -> None:
        client = ctx.http
        if client is None:
            return
        try:
            r = await client.get("/api/v1/dashboard/snapshot", timeout=1.5)
            if r.status_code == 200:
                blob = r.json() if callable(getattr(r, "json", None)) else {}
                self._fc = _safe_dict(blob.get("fc")) if isinstance(blob, dict) else {}
                # Append battery sample for the sparkline.
                bat = _safe_dict(self._fc.get("battery"))
                pct = bat.get("remaining")
                if isinstance(pct, (int, float)):
                    self._battery_history.append(float(pct))
                else:
                    self._battery_history.append(None)
        except Exception as exc:  # noqa: BLE001
            ctx.logger.debug("details_drone_snapshot_fetch_failed", error=str(exc))

    async def render(self, ctx: PageContext) -> Image.Image:
        await self._refresh(ctx)
        palette = ctx.palette
        img = Image.new("RGB", (PAGE_W, PAGE_H), palette.bg_primary)
        draw_header_band(img, palette, "Drone")
        d = ImageDraw.Draw(img)

        paired = _safe_dict(ctx.state.get("paired_drone"))
        device_id = paired.get("device_id")

        if device_id:
            self._render_paired(img, d, palette, paired, ctx)
        else:
            self._render_unpaired(img, d, palette, ctx)
        return img

    def _render_paired(
        self,
        img: Image.Image,
        d: ImageDraw.ImageDraw,
        palette,  # type: ignore[no-untyped-def]
        paired: dict,
        ctx: PageContext,
    ) -> None:
        device_id = str(paired.get("device_id") or "")
        fingerprint = str(paired.get("key_fingerprint") or "")
        paired_at = paired.get("paired_at_seconds")  # optional float seconds
        # Identity row at y=48..96.
        mono = p.font("mono_regular", 11)
        d.text(
            (12, HEADER_H + 8),
            f"id  {device_id}",
            fill=palette.text_primary,
            font=mono,
        )
        if fingerprint:
            short = fingerprint[:16] + ("..." if len(fingerprint) > 16 else "")
        else:
            short = "--"
        d.text(
            (12, HEADER_H + 24),
            f"key {short}",
            fill=palette.text_secondary,
            font=mono,
        )
        if isinstance(paired_at, (int, float)):
            ago_str = _format_relative(paired_at)
        else:
            ago_str = "--"
        d.text(
            (12, HEADER_H + 40),
            f"paired {ago_str}",
            fill=palette.text_tertiary,
            font=mono,
        )

        # 2-column grid below the identity row, y=104..236.
        col_left_x = 12
        col_right_x = 256
        grid_y = HEADER_H + 60

        fc = self._fc or {}
        vehicle = fc.get("vehicle") or "--"
        mode = fc.get("mode") or "--"
        armed = bool(fc.get("armed"))
        battery = _safe_dict(fc.get("battery"))
        bat_v = battery.get("voltage")
        bat_pct = battery.get("remaining")
        gps = _safe_dict(fc.get("gps"))
        gps_fix = gps.get("fix_type")
        gps_sats = gps.get("satellites_visible")

        body_font = p.font("mono_regular", 12)
        body_label = p.font("sans_bold", 10)
        # Left column.
        d.text(
            (col_left_x, grid_y),
            "VEHICLE",
            fill=palette.text_tertiary,
            font=body_label,
        )
        d.text(
            (col_left_x, grid_y + 14),
            str(vehicle).upper(),
            fill=palette.text_primary,
            font=body_font,
        )
        d.text(
            (col_left_x, grid_y + 30),
            f"mode  {mode}",
            fill=palette.text_secondary,
            font=body_font,
        )
        arm_label = "ARMED" if armed else "DISARMED"
        arm_color = palette.status_success if armed else palette.text_secondary
        d.text(
            (col_left_x, grid_y + 46),
            arm_label,
            fill=arm_color,
            font=body_font,
        )
        bat_text = (
            f"bat {bat_v:.1f}V  {int(bat_pct)}%"
            if isinstance(bat_v, (int, float)) and isinstance(bat_pct, (int, float))
            else "bat --"
        )
        d.text(
            (col_left_x, grid_y + 62),
            bat_text,
            fill=palette.text_secondary,
            font=body_font,
        )
        gps_text = (
            f"gps {gps_fix} · {int(gps_sats)} sats"
            if gps_fix is not None and isinstance(gps_sats, (int, float))
            else "gps --"
        )
        d.text(
            (col_left_x, grid_y + 78),
            gps_text,
            fill=palette.text_secondary,
            font=body_font,
        )

        # Right column: 60x60 battery rect + 60-sample sparkline.
        bat_x0 = col_right_x
        bat_y0 = grid_y
        bat_w = 60
        bat_h = 60
        d.rectangle(
            (bat_x0, bat_y0, bat_x0 + bat_w - 1, bat_y0 + bat_h - 1),
            outline=palette.border_strong,
            width=2,
        )
        # Battery cap nub on the right edge.
        d.rectangle(
            (
                bat_x0 + bat_w,
                bat_y0 + bat_h // 4,
                bat_x0 + bat_w + 6,
                bat_y0 + 3 * bat_h // 4,
            ),
            fill=palette.border_strong,
        )
        if isinstance(bat_pct, (int, float)):
            pct = max(0, min(100, int(bat_pct)))
            fill_w = int((bat_w - 4) * pct / 100)
            color = (
                palette.status_success
                if pct >= 50
                else palette.status_warning
                if pct >= 20
                else palette.status_error
            )
            d.rectangle(
                (
                    bat_x0 + 2,
                    bat_y0 + 2,
                    bat_x0 + 2 + fill_w,
                    bat_y0 + bat_h - 3,
                ),
                fill=color,
            )

        # 60s battery sparkline below.
        spark_x = col_right_x + bat_w + 16
        spark_y = grid_y + 4
        spark_w = PAGE_W - spark_x - 12
        spark_h = bat_h - 8
        if self._battery_history:
            draw_sparkline(
                img,
                spark_x,
                spark_y,
                spark_w,
                spark_h,
                list(self._battery_history),
                color=palette.accent_primary,
                y_min=0,
                y_max=100,
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
            cloud.get("pairing_code")
            or cloud.get("pair_code")
            or pairing.get("code")
            or ""
        )

        msg_font = p.font("sans_bold", 14)
        msg = "NOT PAIRED"
        mw, _ = p.text_size(img, msg, msg_font)
        d.text(
            ((PAGE_W - mw) // 2, HEADER_H + 8),
            msg,
            fill=palette.text_secondary,
            font=msg_font,
        )

        if code:
            code_font = p.font("mono_bold", 22)
            cw, ch = p.text_size(img, code, code_font)
            d.text(
                ((PAGE_W - cw) // 2, HEADER_H + 32),
                code,
                fill=palette.text_primary,
                font=code_font,
            )
            qr_payload = (
                cloud.get("pair_url")
                or f"altnautica.com/command?pair={code}"
            )
            qr = render_qr(str(qr_payload), target_px=100)
            if qr is not None:
                qr_x = (PAGE_W - 100) // 2
                qr_y = HEADER_H + 60
                img.paste(qr, (qr_x, qr_y))

        # Open pairing button at y=178..226 centered, 200x40.
        btn_w = 200
        btn_h = 40
        btn_x = (PAGE_W - btn_w) // 2
        btn_y = 188
        d.rectangle(
            (btn_x, btn_y, btn_x + btn_w - 1, btn_y + btn_h - 1),
            fill=palette.accent_primary,
            outline=palette.text_primary,
            width=1,
        )
        btn_label = "Open pairing window"
        btn_font = p.font("sans_bold", 12)
        bw, bh = p.text_size(img, btn_label, btn_font)
        d.text(
            (btn_x + (btn_w - bw) // 2, btn_y + (btn_h - bh) // 2 - 1),
            btn_label,
            fill=palette.text_primary,
            font=btn_font,
        )

    def hit_zones(self, ctx: PageContext) -> list[HitZone]:
        zones: list[HitZone] = [HitZone(id="details.back", x=8, y=8, w=40, h=32)]
        paired = _safe_dict(ctx.state.get("paired_drone"))
        if not paired.get("device_id"):
            zones.append(
                HitZone(id="drone.open_pairing", x=140, y=188, w=200, h=40),
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
        if zone.id == "drone.open_pairing" and gesture.kind == "tap":
            client = ctx.http
            ctx.logger.info("details_drone_open_pairing")
            if client is None:
                return
            try:
                await client.post(
                    "/api/v1/pair/local-bind",
                    json={},
                    timeout=2.0,
                )
            except Exception as exc:  # noqa: BLE001
                ctx.logger.debug(
                    "details_drone_open_pairing_failed", error=str(exc)
                )
