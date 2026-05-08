"""Uplink detail page.

Drilldown opened from the dashboard's UPLINK / CLOUD tile. Shows the
cloud-relay state on top and a cellular block (or WiFi fallback) on
the bottom.

Data sources:

* ``GET /api/v1/dashboard/snapshot`` — ``cloud`` block with
  mqtt_state / http_state / rtt_ms / drone_id / pairing_code.
* ``GET /api/v1/ground-station/modem-status`` — modem present plus
  RSRP / RSRQ / SINR / band / IP / tech.

WiFi fallback:

* When ``modem-status`` returns ``present: false``, fall back to
  ``state.network.wifi_client`` for SSID + signal. If neither is
  available render "No WAN uplink" muted.
"""

from __future__ import annotations

import asyncio
from typing import Any, ClassVar

from PIL import Image, ImageDraw

from ados.services.ui.dashboards.components import primitives as p
from ados.services.ui.touch.events import TouchGesture

from ..base import HitZone, PageContext
from ._common import HEADER_H, draw_header_band

PAGE_W = 480
PAGE_H = 244

CLOUD_BAND_Y = HEADER_H + 4
CLOUD_BAND_H = 86
CELL_BAND_Y = CLOUD_BAND_Y + CLOUD_BAND_H + 4
CELL_BAND_H = PAGE_H - CELL_BAND_Y - 12


def _safe_dict(value: Any) -> dict:
    return value if isinstance(value, dict) else {}


def _bars_for_rsrp(rsrp_dbm: float | None) -> int:
    if rsrp_dbm is None:
        return 0
    if rsrp_dbm >= -90:
        return 4
    if rsrp_dbm >= -100:
        return 3
    if rsrp_dbm >= -110:
        return 2
    return 1


def _cloud_state_label(cloud: dict) -> tuple[str, str]:
    """Return (badge_label, severity) for the cloud state.

    severity is one of ``ok`` / ``warn`` / ``err`` / ``muted``.
    """
    mqtt = (cloud.get("mqtt_state") or "").lower()
    http = (cloud.get("http_state") or "").lower()
    paired = bool(cloud.get("pairing_code")) is False and bool(cloud.get("drone_id"))
    if not paired and not cloud.get("drone_id"):
        return "UNPAIRED", "muted"
    if mqtt == "connected" and http in ("ok", "connected"):
        return "CONNECTED", "ok"
    if mqtt in ("connecting", "reconnecting") or http in ("connecting", "reconnecting"):
        return "RECONNECTING", "warn"
    return "OFFLINE", "err"


class UplinkDetailPage:
    """Detail view for the UPLINK / CLOUD dashboard tile."""

    id: ClassVar[str] = "details.uplink"
    refresh_hz: ClassVar[float] = 2.0

    def __init__(self) -> None:
        self._cloud: dict[str, Any] = {}
        self._modem: dict[str, Any] = {"present": False}
        self._fetch_task: asyncio.Task | None = None

    async def on_enter(self, ctx: PageContext) -> None:
        ctx.logger.info("details_uplink_enter")
        await self._refresh(ctx)

    async def on_leave(self, ctx: PageContext) -> None:
        ctx.logger.info("details_uplink_leave")
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
                self._cloud = _safe_dict(blob.get("cloud")) if isinstance(blob, dict) else {}
        except Exception as exc:  # noqa: BLE001
            ctx.logger.debug("details_uplink_snapshot_fetch_failed", error=str(exc))
        try:
            r = await client.get(
                "/api/v1/ground-station/modem-status",
                timeout=1.5,
            )
            if r.status_code == 200:
                blob = r.json() if callable(getattr(r, "json", None)) else {}
                self._modem = blob if isinstance(blob, dict) else {"present": False}
        except Exception as exc:  # noqa: BLE001
            ctx.logger.debug("details_uplink_modem_fetch_failed", error=str(exc))

    async def render(self, ctx: PageContext) -> Image.Image:
        await self._refresh(ctx)
        palette = ctx.palette
        img = Image.new("RGB", (PAGE_W, PAGE_H), palette.bg_primary)
        draw_header_band(img, palette, "Uplink")
        d = ImageDraw.Draw(img)

        cloud = self._cloud or _safe_dict(ctx.state.get("cloud"))
        self._render_cloud_band(img, d, palette, cloud)
        if self._modem.get("present"):
            self._render_cellular_band(img, d, palette, self._modem)
        else:
            self._render_wifi_fallback(img, d, palette, ctx)
        return img

    def _render_cloud_band(
        self,
        img: Image.Image,
        d: ImageDraw.ImageDraw,
        palette,  # type: ignore[no-untyped-def]
        cloud: dict,
    ) -> None:
        # Status badge + dot row.
        label, severity = _cloud_state_label(cloud)
        if severity == "ok":
            color = palette.status_success
        elif severity == "warn":
            color = palette.status_warning
        elif severity == "err":
            color = palette.status_error
        else:
            color = palette.text_tertiary

        d.ellipse((12, CLOUD_BAND_Y + 6, 22, CLOUD_BAND_Y + 16), fill=color)
        title_font = p.font("sans_bold", 14)
        d.text(
            (28, CLOUD_BAND_Y + 4),
            label,
            fill=palette.text_primary,
            font=title_font,
        )

        body_font = p.font("mono_regular", 11)
        mqtt = cloud.get("mqtt_state") or "--"
        http = cloud.get("http_state") or "--"
        rtt = cloud.get("rtt_ms")
        rtt_text = f"{int(rtt)} ms" if isinstance(rtt, (int, float)) else "-- ms"
        d.text(
            (12, CLOUD_BAND_Y + 28),
            f"mqtt {mqtt}",
            fill=palette.text_secondary,
            font=body_font,
        )
        d.text(
            (12, CLOUD_BAND_Y + 44),
            f"http {http}",
            fill=palette.text_secondary,
            font=body_font,
        )
        d.text(
            (12, CLOUD_BAND_Y + 60),
            f"rtt  {rtt_text}",
            fill=palette.text_secondary,
            font=body_font,
        )

        drone_id = str(cloud.get("drone_id") or "")
        pairing_code = str(cloud.get("pairing_code") or "")
        right_x = 240
        if drone_id:
            d.text(
                (right_x, CLOUD_BAND_Y + 28),
                f"id {drone_id}",
                fill=palette.text_secondary,
                font=body_font,
            )
        if pairing_code and not drone_id:
            d.text(
                (right_x, CLOUD_BAND_Y + 44),
                f"pair {pairing_code}",
                fill=palette.text_primary,
                font=p.font("mono_bold", 12),
            )

    def _render_cellular_band(
        self,
        img: Image.Image,
        d: ImageDraw.ImageDraw,
        palette,  # type: ignore[no-untyped-def]
        modem: dict,
    ) -> None:
        rsrp = modem.get("rsrp_dbm")
        rsrq = modem.get("rsrq_db")
        sinr = modem.get("sinr_db")
        band = modem.get("band") or "--"
        ip = modem.get("ip") or "--"
        tech = (modem.get("tech") or "--").upper()
        bars = _bars_for_rsrp(rsrp if isinstance(rsrp, (int, float)) else None)

        # Signal bars graphic at left.
        bars_x = 12
        bars_y = CELL_BAND_Y + 8
        bar_w = 8
        bar_gap = 3
        for i in range(4):
            h = 8 + i * 6
            x0 = bars_x + i * (bar_w + bar_gap)
            y0 = bars_y + (28 - h)
            color = (
                palette.accent_primary
                if i < bars
                else palette.bg_tertiary
            )
            d.rectangle(
                (x0, y0, x0 + bar_w - 1, bars_y + 28),
                fill=color,
                outline=palette.border_default,
                width=1,
            )

        body_font = p.font("mono_regular", 11)
        rsrp_text = f"rsrp {int(rsrp)}" if isinstance(rsrp, (int, float)) else "rsrp --"
        rsrq_text = f"rsrq {int(rsrq)}" if isinstance(rsrq, (int, float)) else "rsrq --"
        sinr_text = f"sinr {int(sinr)}" if isinstance(sinr, (int, float)) else "sinr --"
        text_x = bars_x + 4 * (bar_w + bar_gap) + 16
        d.text(
            (text_x, CELL_BAND_Y + 6),
            f"{tech} · band {band}",
            fill=palette.text_primary,
            font=p.font("sans_bold", 12),
        )
        d.text(
            (text_x, CELL_BAND_Y + 22),
            f"{rsrp_text} · {rsrq_text} · {sinr_text}",
            fill=palette.text_secondary,
            font=body_font,
        )
        d.text(
            (text_x, CELL_BAND_Y + 38),
            f"ip {ip}",
            fill=palette.text_secondary,
            font=body_font,
        )

    def _render_wifi_fallback(
        self,
        img: Image.Image,
        d: ImageDraw.ImageDraw,
        palette,  # type: ignore[no-untyped-def]
        ctx: PageContext,
    ) -> None:
        network = _safe_dict(ctx.state.get("network"))
        wifi = _safe_dict(network.get("wifi_client"))
        connected = bool(wifi.get("connected"))
        body_font = p.font("mono_regular", 12)
        if connected:
            ssid = wifi.get("ssid") or "--"
            signal = wifi.get("signal_dbm")
            sig_text = (
                f"signal {int(signal)} dBm"
                if isinstance(signal, (int, float))
                else "signal --"
            )
            d.text(
                (12, CELL_BAND_Y + 12),
                f"WiFi uplink: {ssid}",
                fill=palette.text_primary,
                font=p.font("sans_bold", 12),
            )
            d.text(
                (12, CELL_BAND_Y + 30),
                sig_text,
                fill=palette.text_secondary,
                font=body_font,
            )
        else:
            msg = "No WAN uplink"
            d.text(
                (12, CELL_BAND_Y + 14),
                msg,
                fill=palette.text_tertiary,
                font=p.font("sans_bold", 12),
            )
            reason = self._modem.get("reason") if isinstance(self._modem, dict) else None
            if reason:
                d.text(
                    (12, CELL_BAND_Y + 32),
                    f"modem: {reason}",
                    fill=palette.text_tertiary,
                    font=body_font,
                )

    def hit_zones(self, ctx: PageContext) -> list[HitZone]:
        return [HitZone(id="details.back", x=8, y=8, w=40, h=32)]

    async def on_touch(
        self,
        ctx: PageContext,
        zone: HitZone,
        gesture: TouchGesture,
    ) -> None:
        if zone.id == "details.back" and gesture.kind == "tap":
            await ctx.navigator.pop_modal(ctx=ctx)
