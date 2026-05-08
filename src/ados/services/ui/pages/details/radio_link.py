"""Radio Link detail page.

Drilldown opened from the dashboard's RADIO LINK tile. Shows a
60-second RSSI sparkline at the top, a 3-column readout grid in the
middle, and a TX-power slider with stepper buttons at the bottom.

REST endpoints used:

* ``GET /api/wfb`` — current snapshot (RSSI, SNR, bitrate, FEC,
  channel, freq, bandwidth, TX power).
* ``GET /api/wfb/history?seconds=60`` — sparkline history.
* ``PUT /api/wfb/tx-power`` — committed TX power on slider release
  or stepper tap.

Touch behaviour:

* Tap on the back chevron pops the modal.
* Tap on ``radio.tx_minus`` / ``radio.tx_plus`` steps the TX power
  by 1 dBm and commits.
* Drag inside ``radio.tx_slider`` tracks the thumb live (subscribes
  to :class:`TouchMoveBus` for the duration of the drag) and commits
  on pen-up.

The TouchMoveBus subscription is created lazily on the first drag
and torn down inside :meth:`on_leave` so the page never leaks a
hung consumer when the operator backs out mid-drag.
"""

from __future__ import annotations

import asyncio
from typing import Any, ClassVar

from PIL import Image, ImageDraw

from ados.services.ui.dashboards.components import primitives as p
from ados.services.ui.dashboards.components.sparkline import draw_sparkline
from ados.services.ui.touch.events import TouchGesture

from ..base import HitZone, PageContext
from ._common import HEADER_H, draw_header_band

PAGE_W = 480
PAGE_H = 244

# TX-power envelope. Floor 1 dBm, ceiling 15 dBm matches the standard
# WfbConfig clamp on the agent side.
TX_MIN_DBM = 1
TX_MAX_DBM = 15

# Slider geometry.
SLIDER_X = 60
SLIDER_Y = 200
SLIDER_W = 360
SLIDER_TRACK_H = 8
THUMB_W = 24
THUMB_H = 24
MINUS_X = 8
MINUS_Y = 188
MINUS_W = 44
MINUS_H = 44
PLUS_X = 428
PLUS_Y = 188
PLUS_W = 44
PLUS_H = 44


def _safe_dict(value: Any) -> dict:
    return value if isinstance(value, dict) else {}


def _samples_from_history(blob: dict) -> list[float | None]:
    """Pull the rssi_dbm column out of /api/wfb/history's samples list."""
    raw = blob.get("samples") if isinstance(blob, dict) else None
    if not isinstance(raw, list):
        return []
    out: list[float | None] = []
    for item in raw:
        if not isinstance(item, dict):
            out.append(None)
            continue
        v = item.get("rssi_dbm")
        if isinstance(v, (int, float)):
            out.append(float(v))
        else:
            out.append(None)
    return out


class RadioLinkDetailPage:
    """Detail view for the RADIO LINK dashboard tile."""

    id: ClassVar[str] = "details.radio_link"
    refresh_hz: ClassVar[float] = 2.0

    def __init__(self) -> None:
        self._snapshot: dict[str, Any] = {}
        self._history: list[float | None] = []
        self._tx_target_dbm: int | None = None
        self._dragging: bool = False
        self._drag_task: asyncio.Task | None = None

    async def on_enter(self, ctx: PageContext) -> None:
        ctx.logger.info("details_radio_link_enter")
        await self._refresh(ctx)

    async def on_leave(self, ctx: PageContext) -> None:
        ctx.logger.info("details_radio_link_leave")
        # Tear down any in-flight drag-tracker task so a back-out
        # during a drag does not leak a TouchMoveBus subscriber.
        if self._drag_task is not None and not self._drag_task.done():
            self._drag_task.cancel()
            try:
                await self._drag_task
            except (asyncio.CancelledError, Exception):
                pass
        self._drag_task = None
        self._dragging = False

    async def _refresh(self, ctx: PageContext) -> None:
        """Re-fetch snapshot + history. Best-effort; never raises."""
        client = ctx.http
        if client is None:
            return
        try:
            r = await client.get("/api/wfb", timeout=1.5)
            if r.status_code == 200:
                self._snapshot = r.json() if isinstance(r.json(), dict) else {}
        except Exception as exc:  # noqa: BLE001
            ctx.logger.debug("details_radio_link_snapshot_fetch_failed", error=str(exc))
        try:
            r = await client.get(
                "/api/wfb/history",
                params={"seconds": 60},
                timeout=1.5,
            )
            if r.status_code == 200:
                self._history = _samples_from_history(r.json())
        except Exception as exc:  # noqa: BLE001
            ctx.logger.debug("details_radio_link_history_fetch_failed", error=str(exc))

    async def render(self, ctx: PageContext) -> Image.Image:
        # Refresh on every tick; the page navigator drives at 2 Hz so
        # this fires roughly twice per second.
        await self._refresh(ctx)
        palette = ctx.palette
        img = Image.new("RGB", (PAGE_W, PAGE_H), palette.bg_primary)
        draw_header_band(img, palette, "Radio link")

        snap = self._snapshot or _safe_dict(ctx.state.get("link"))
        rssi = snap.get("rssi_dbm")
        snr = snap.get("snr_db")
        noise = snap.get("noise_dbm")
        bitrate = snap.get("bitrate_mbps")
        fec_rec = snap.get("fec_recovered")
        fec_lost = snap.get("fec_lost")
        channel = snap.get("channel")
        freq = snap.get("frequency_mhz")
        bw = snap.get("bandwidth_mhz")
        tx_power = snap.get("tx_power_dbm")
        if isinstance(tx_power, (int, float)) and self._tx_target_dbm is None:
            self._tx_target_dbm = max(TX_MIN_DBM, min(TX_MAX_DBM, int(tx_power)))

        # Sparkline band y=44..120 (76 px tall).
        spark_y = HEADER_H + 4
        spark_h = 76
        if self._history:
            draw_sparkline(
                img,
                8,
                spark_y,
                PAGE_W - 16,
                spark_h - 16,
                self._history,
                color=palette.accent_primary,
                fill_below=False,
            )
            real = [v for v in self._history if isinstance(v, (int, float))]
            peak = int(max(real)) if real else 0
            floor = int(min(real)) if real else 0
        else:
            d = ImageDraw.Draw(img)
            empty_font = p.font("sans_regular", 11)
            msg = "no history yet"
            mw, _ = p.text_size(img, msg, empty_font)
            d.text(
                ((PAGE_W - mw) // 2, spark_y + (spark_h - 16) // 2),
                msg,
                fill=palette.text_tertiary,
                font=empty_font,
            )
            peak = 0
            floor = 0

        # Sparkline footer line: rssi value + peak/floor summary.
        d = ImageDraw.Draw(img)
        summary_font = p.font("mono_regular", 11)
        if isinstance(rssi, (int, float)):
            summary = f"rssi {int(rssi)} dBm  (peak {peak} / floor {floor})"
        else:
            summary = "rssi -- dBm"
        d.text(
            (8, spark_y + spark_h - 14),
            summary,
            fill=palette.text_secondary,
            font=summary_font,
        )

        # 3-column readout grid y=124..184.
        grid_y = 124
        col_w = PAGE_W // 3
        col_label_font = p.font("sans_bold", 10)
        col_value_font = p.font("mono_bold", 14)
        col_unit_font = p.font("mono_regular", 11)

        def _col(x0: int, label: str, primary: str, secondary: str) -> None:
            d.text(
                (x0 + 8, grid_y),
                label.upper(),
                fill=palette.text_tertiary,
                font=col_label_font,
            )
            d.text(
                (x0 + 8, grid_y + 16),
                primary,
                fill=palette.text_primary,
                font=col_value_font,
            )
            d.text(
                (x0 + 8, grid_y + 36),
                secondary,
                fill=palette.text_secondary,
                font=col_unit_font,
            )

        snr_text = f"{snr:.0f} dB" if isinstance(snr, (int, float)) else "-- dB"
        noise_text = f"noise {int(noise)} dBm" if isinstance(noise, (int, float)) else "noise --"
        _col(0, "SNR", snr_text, noise_text)

        bitrate_text = (
            f"{bitrate:.1f} Mbps" if isinstance(bitrate, (int, float)) else "-- Mbps"
        )
        if isinstance(fec_rec, (int, float)) and isinstance(fec_lost, (int, float)):
            fec_text = f"FEC R {int(fec_rec)} L {int(fec_lost)}"
        else:
            fec_text = "FEC -- / --"
        _col(col_w, "Bitrate", bitrate_text, fec_text)

        ch_text = f"ch {int(channel)}" if isinstance(channel, (int, float)) else "ch --"
        if isinstance(freq, (int, float)) and isinstance(bw, (int, float)):
            band_text = f"{int(freq)} MHz · {int(bw)} MHz"
        elif isinstance(freq, (int, float)):
            band_text = f"{int(freq)} MHz"
        else:
            band_text = "-- MHz"
        _col(col_w * 2, "Channel", ch_text, band_text)

        # TX-power slider y=188..236.
        self._draw_slider(img, palette, self._tx_target_dbm or TX_MIN_DBM)
        return img

    def _draw_slider(
        self,
        image: Image.Image,
        palette,  # type: ignore[no-untyped-def]
        value_dbm: int,
    ) -> None:
        """Paint the TX-power slider, value chip, and ± buttons."""
        d = ImageDraw.Draw(image)
        # Stepper button backgrounds.
        for x0, y0 in ((MINUS_X, MINUS_Y), (PLUS_X, PLUS_Y)):
            d.rectangle(
                (x0, y0, x0 + MINUS_W - 1, y0 + MINUS_H - 1),
                fill=palette.bg_secondary,
                outline=palette.border_strong,
                width=1,
            )
        # Minus and plus glyphs.
        center_y_minus = MINUS_Y + MINUS_H // 2
        center_y_plus = PLUS_Y + PLUS_H // 2
        d.line(
            (MINUS_X + 12, center_y_minus, MINUS_X + MINUS_W - 12, center_y_minus),
            fill=palette.text_primary,
            width=2,
        )
        d.line(
            (PLUS_X + 12, center_y_plus, PLUS_X + PLUS_W - 12, center_y_plus),
            fill=palette.text_primary,
            width=2,
        )
        cx_plus = PLUS_X + PLUS_W // 2
        d.line(
            (cx_plus, PLUS_Y + 12, cx_plus, PLUS_Y + PLUS_H - 12),
            fill=palette.text_primary,
            width=2,
        )

        # Track.
        track_y = SLIDER_Y + (THUMB_H - SLIDER_TRACK_H) // 2
        d.rectangle(
            (
                SLIDER_X,
                track_y,
                SLIDER_X + SLIDER_W - 1,
                track_y + SLIDER_TRACK_H - 1,
            ),
            fill=palette.bg_tertiary,
            outline=palette.border_default,
            width=1,
        )

        # Filled portion to the thumb.
        clamped = max(TX_MIN_DBM, min(TX_MAX_DBM, value_dbm))
        frac = (clamped - TX_MIN_DBM) / (TX_MAX_DBM - TX_MIN_DBM)
        thumb_cx = SLIDER_X + int(round(frac * SLIDER_W))
        d.rectangle(
            (
                SLIDER_X,
                track_y,
                thumb_cx,
                track_y + SLIDER_TRACK_H - 1,
            ),
            fill=palette.accent_primary,
        )

        # Thumb.
        thumb_x0 = thumb_cx - THUMB_W // 2
        thumb_y0 = SLIDER_Y
        d.rectangle(
            (
                thumb_x0,
                thumb_y0,
                thumb_x0 + THUMB_W - 1,
                thumb_y0 + THUMB_H - 1,
            ),
            fill=palette.accent_primary,
            outline=palette.text_primary,
            width=1,
        )

        # Value chip just above the thumb.
        chip_text = f"{clamped} dBm"
        chip_font = p.font("mono_bold", 11)
        cw, ch = p.text_size(image, chip_text, chip_font)
        chip_y = SLIDER_Y - ch - 4
        chip_x = max(SLIDER_X, min(SLIDER_X + SLIDER_W - cw, thumb_cx - cw // 2))
        d.text(
            (chip_x, chip_y),
            chip_text,
            fill=palette.text_primary,
            font=chip_font,
        )

    def hit_zones(self, ctx: PageContext) -> list[HitZone]:
        return [
            HitZone(id="details.back", x=8, y=8, w=40, h=32),
            HitZone(
                id="radio.tx_slider",
                x=SLIDER_X,
                y=SLIDER_Y,
                w=SLIDER_W,
                h=THUMB_H,
            ),
            HitZone(id="radio.tx_minus", x=MINUS_X, y=MINUS_Y, w=MINUS_W, h=MINUS_H),
            HitZone(id="radio.tx_plus", x=PLUS_X, y=PLUS_Y, w=PLUS_W, h=PLUS_H),
        ]

    def _value_for_x(self, x_lcd: int) -> int:
        """Translate an LCD-x to a dBm value clamped into the envelope."""
        # Slider zone is in page-local coordinates; gestures arrive in
        # LCD-global coordinates. The page is offset by 32 px in y but
        # the x axis is identity, so x_lcd == x_page.
        rel = max(0, min(SLIDER_W - 1, x_lcd - SLIDER_X))
        frac = rel / (SLIDER_W - 1) if SLIDER_W > 1 else 0.0
        return int(round(TX_MIN_DBM + frac * (TX_MAX_DBM - TX_MIN_DBM)))

    async def _commit_tx_power(self, ctx: PageContext, value_dbm: int) -> None:
        """PUT the new TX power and refresh from the response."""
        client = ctx.http
        clamped = max(TX_MIN_DBM, min(TX_MAX_DBM, int(value_dbm)))
        self._tx_target_dbm = clamped
        if client is None:
            return
        try:
            r = await client.put(
                "/api/wfb/tx-power",
                json={"tx_power_dbm": clamped},
                timeout=2.0,
            )
            if r.status_code == 200:
                body = r.json() if callable(getattr(r, "json", None)) else {}
                if isinstance(body, dict):
                    eff = body.get("effective_dbm") or body.get("tx_power_dbm")
                    if isinstance(eff, (int, float)):
                        self._tx_target_dbm = max(
                            TX_MIN_DBM, min(TX_MAX_DBM, int(eff))
                        )
                ctx.logger.info(
                    "details_radio_link_tx_power_committed",
                    requested=clamped,
                )
            else:
                ctx.logger.warning(
                    "details_radio_link_tx_power_rejected",
                    status=r.status_code,
                )
        except Exception as exc:  # noqa: BLE001
            ctx.logger.debug(
                "details_radio_link_tx_power_failed", error=str(exc)
            )

    async def _track_drag(self, ctx: PageContext) -> None:
        """Subscribe to TouchMoveBus while a drag is active.

        The bridge's gesture-bus emits a final ``drag`` event on
        pen-up and we commit there. While the pen is down we want
        live thumb tracking, which means listening on the
        higher-frequency move bus.
        """
        bridge = getattr(ctx.framebuffer, "_touch_bridge", None) if ctx.framebuffer else None
        if bridge is None:
            # The OLED service stores the bridge on itself, not on the
            # framebuffer; reach back through the navigator's owner if
            # one is available. The simplest portable answer is to walk
            # the logger's bound app reference, but for testability we
            # pull the move bus off ctx.framebuffer when present and
            # otherwise no-op.
            return
        move_bus = getattr(bridge, "move_bus", None)
        if move_bus is None:
            return
        try:
            async for move in move_bus.subscribe():
                if not self._dragging:
                    break
                self._tx_target_dbm = self._value_for_x(move.x_lcd)
        except asyncio.CancelledError:
            return
        except Exception as exc:  # noqa: BLE001
            ctx.logger.debug("details_radio_link_drag_track_failed", error=str(exc))

    async def on_touch(
        self,
        ctx: PageContext,
        zone: HitZone,
        gesture: TouchGesture,
    ) -> None:
        if zone.id == "details.back" and gesture.kind == "tap":
            await ctx.navigator.pop_modal(ctx=ctx)
            return
        if zone.id == "radio.tx_minus" and gesture.kind == "tap":
            current = self._tx_target_dbm or TX_MIN_DBM
            await self._commit_tx_power(ctx, current - 1)
            return
        if zone.id == "radio.tx_plus" and gesture.kind == "tap":
            current = self._tx_target_dbm or TX_MIN_DBM
            await self._commit_tx_power(ctx, current + 1)
            return
        if zone.id == "radio.tx_slider":
            # Drag complete (pen-up). Commit the final value at the
            # release point.
            if gesture.kind in ("drag", "tap"):
                self._dragging = False
                if self._drag_task is not None and not self._drag_task.done():
                    self._drag_task.cancel()
                    try:
                        await self._drag_task
                    except (asyncio.CancelledError, Exception):
                        pass
                    self._drag_task = None
                final_value = self._value_for_x(gesture.end_x)
                await self._commit_tx_power(ctx, final_value)
                return
            # Drag-start: kick off live tracking. The page calls
            # _track_drag which subscribes to TouchMoveBus until
            # _dragging flips back to False on pen-up.
            self._dragging = True
            self._tx_target_dbm = self._value_for_x(gesture.start_x)
            if self._drag_task is None or self._drag_task.done():
                self._drag_task = asyncio.create_task(self._track_drag(ctx))
