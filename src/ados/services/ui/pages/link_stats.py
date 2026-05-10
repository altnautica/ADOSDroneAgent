"""Link Stats page — live radio + decoder + system metrics on the LCD.

Replaces the old More/"+" tab with an at-a-glance diagnostics surface
that catches the failure modes operators actually hit on the bench:

* wfb_tx alive but the radio isn't transmitting (counter delta = 0)
* wfb_rx alive but stdout silent (RX silence watchdog state)
* mediamtx /main "ready" with inboundBytes flat
* decoder reaching PLAYING but fps == 0
* SoC over-temp, RAM saturation

Three vertical bands fill the 480x244 page body:

1. **LINK** (top) — state pill, channel, RSSI, packets/loss, FEC,
   bitrate, plus a 60 s RSSI sparkline.
2. **DECODER + STREAM** (middle) — decoder kind, fps, glass-to-glass
   latency, mediamtx ready/inbound rate, recording state.
3. **SYSTEM** (bottom) — CPU%, RAM%, Disk%, SoC temp.

All values refresh at ~1 Hz from inside ``render()``; a single
``_last_refresh_at`` gate throttles the four upstream HTTP/file reads
so the render tick stays cheap. Sparkline samples are appended only
on a successful refresh so a stalled poll doesn't pollute the trend
line with stale values (we record None instead — see sparkline.draw_
which renders Nones as gaps).

Color coding (via ``ctx.palette`` + threshold helper): green when
in spec, yellow when degraded, red when broken. Thresholds match
the operating envelope of the hardware (RSSI < -80 dBm, fps < 20,
temp > 75°C, etc.).

The page deliberately does NOT push drilldown modals on tap — every
metric is visible inline; an operator who wants more detail navigates
to Settings → Maintenance → Diagnostics for the full system panel.
This keeps the page a pure read-only watch surface.
"""

from __future__ import annotations

import asyncio
import json
import time
from collections import deque
from pathlib import Path
from typing import Any, ClassVar

from PIL import Image, ImageDraw

from ados.services.ui.dashboards.components import primitives as p
from ados.services.ui.dashboards.components.sparkline import draw_sparkline
from ados.services.ui.pages.base import HitZone, PageContext
from ados.services.ui.touch.events import TouchGesture

PAGE_W = 480
PAGE_H = 244

# Section heights (top to bottom). The numbers add to PAGE_H exactly.
_LINK_H = 100
_DEC_H = 76
_SYS_H = PAGE_H - _LINK_H - _DEC_H  # 68 px

# Refresh cadence inside render(). Each upstream is gated by this so
# we don't hit /api/wfb four times per render at refresh_hz=5.
_REFRESH_INTERVAL_S = 1.0

# 60 samples at 1 Hz = 60 s of history.
_HISTORY_LEN = 60

# Thresholds for the green/yellow/red color tiering. Picked to match
# the bench operating envelope so the page reads as alarming only
# when the operator should actually be alarmed.
_RSSI_OK_DBM = -65.0
_RSSI_WARN_DBM = -80.0
_FPS_OK = 25.0
_FPS_WARN = 15.0
_TEMP_OK_C = 65.0
_TEMP_WARN_C = 75.0
_LOSS_OK = 1.0
_LOSS_WARN = 5.0
_MEM_WARN = 80.0
_MEM_CRIT = 90.0


def _safe_dict(value: Any) -> dict:
    return value if isinstance(value, dict) else {}


def _color_for(
    value: float | None,
    *,
    ok: float,
    warn: float,
    palette,
    higher_is_better: bool = True,
) -> tuple[int, int, int]:
    """Return a green/yellow/red color tuple based on threshold tier.

    ``higher_is_better=True``: value >= ok = green, >= warn = yellow,
    else red. ``higher_is_better=False``: value <= ok = green, <= warn
    = yellow, else red.
    """
    if value is None:
        return p.TEXT_SECONDARY
    if higher_is_better:
        if value >= ok:
            return palette.status_success
        if value >= warn:
            return palette.status_warning
        return palette.status_error
    # lower-is-better
    if value <= ok:
        return palette.status_success
    if value <= warn:
        return palette.status_warning
    return palette.status_error


class LinkStatsPage:
    """Live link + decoder + system metrics — replaces the More tab."""

    id: ClassVar[str] = "link_stats"
    refresh_hz: ClassVar[float] = 2.0

    def __init__(self) -> None:
        self._wfb: dict[str, Any] = {}
        self._mtx: dict[str, Any] = {}
        self._tap: dict[str, Any] = {}
        self._health: dict[str, Any] = {}
        self._mtx_inbound_prev: int | None = None
        self._mtx_inbound_kbps: float | None = None
        self._mtx_inbound_at: float = 0.0
        self._rssi_history: deque[float | None] = deque(maxlen=_HISTORY_LEN)
        self._last_refresh_at: float = 0.0

    # ── lifecycle ──────────────────────────────────────────────

    async def on_enter(self, ctx: PageContext) -> None:
        ctx.logger.info("link_stats_enter")
        # Force the first refresh so the page lands populated rather
        # than blank for the first render tick.
        await self._refresh(ctx, force=True)

    async def on_leave(self, ctx: PageContext) -> None:
        ctx.logger.info("link_stats_leave")

    # ── refresh ────────────────────────────────────────────────

    async def _refresh(self, ctx: PageContext, *, force: bool = False) -> None:
        now = time.monotonic()
        if not force and (now - self._last_refresh_at) < _REFRESH_INTERVAL_S:
            return
        self._last_refresh_at = now

        # /api/wfb returns the full radio status. Both drone and GS
        # profiles surface this endpoint with the same shape (the GS
        # variant goes through ground_station.wfb_rx.WfbRxManager.stats()
        # but the keys match WfbManager.stats() on the air side).
        if ctx.http is not None:
            try:
                r = await ctx.http.get("/api/wfb", timeout=1.5)
                if r.status_code == 200 and isinstance(r.json(), dict):
                    self._wfb = r.json()
            except Exception as exc:  # noqa: BLE001
                ctx.logger.debug("link_stats_wfb_fetch_failed", error=str(exc))

        # mediamtx control API. Try the canonical port; some installs
        # disable the API in which case we render mediamtx state as
        # unknown rather than crashing.
        await self._refresh_mediamtx(ctx)

        # /run/ados/lcd-video-tap.json is written by LocalVideoTap on
        # the OLED service — single small file, tolerate read errors.
        self._tap = self._read_run_json("/run/ados/lcd-video-tap.json")
        self._health = self._read_run_json("/run/ados/health.json")

        # Sparkline samples — record None on a missing reading so the
        # gap is visible.
        rssi = self._wfb.get("rssi_dbm") if self._wfb else None
        self._rssi_history.append(
            float(rssi) if isinstance(rssi, (int, float)) else None
        )

    async def _refresh_mediamtx(self, ctx: PageContext) -> None:
        if ctx.http is None:
            return
        try:
            # Local mediamtx control plane is on 9997. The OLED service's
            # ctx.http base_url is the agent's own /api, so we use a raw
            # absolute URL here.
            r = await ctx.http.get(
                "http://127.0.0.1:9997/v3/paths/get/main",
                timeout=1.0,
            )
            if r.status_code != 200:
                return
            data = r.json() if callable(getattr(r, "json", None)) else {}
            if not isinstance(data, dict):
                return
            self._mtx = data
            inbound = data.get("inboundBytes")
            now = time.monotonic()
            if (
                isinstance(inbound, int)
                and self._mtx_inbound_prev is not None
                and self._mtx_inbound_at > 0
            ):
                dt = now - self._mtx_inbound_at
                if dt > 0:
                    delta = max(inbound - self._mtx_inbound_prev, 0)
                    self._mtx_inbound_kbps = (delta * 8.0) / 1000.0 / dt
            if isinstance(inbound, int):
                self._mtx_inbound_prev = inbound
                self._mtx_inbound_at = now
        except Exception as exc:  # noqa: BLE001
            ctx.logger.debug(
                "link_stats_mtx_fetch_failed", error=str(exc)
            )

    @staticmethod
    def _read_run_json(path: str) -> dict:
        try:
            with open(path) as f:
                blob = json.load(f)
                return blob if isinstance(blob, dict) else {}
        except (FileNotFoundError, ValueError, OSError):
            return {}

    # ── render ─────────────────────────────────────────────────

    async def render(self, ctx: PageContext) -> Image.Image:
        await self._refresh(ctx)
        palette = ctx.palette
        img = Image.new("RGB", (PAGE_W, PAGE_H), palette.bg_primary)
        self._draw_link_band(img, palette, x=0, y=0, w=PAGE_W, h=_LINK_H)
        self._draw_dec_band(
            img, palette, x=0, y=_LINK_H, w=PAGE_W, h=_DEC_H,
        )
        self._draw_sys_band(
            img,
            palette,
            x=0,
            y=_LINK_H + _DEC_H,
            w=PAGE_W,
            h=_SYS_H,
        )
        return img

    # ── band drawers ────────────────────────────────────────────

    def _draw_link_band(
        self, img, palette, *, x: int, y: int, w: int, h: int,
    ) -> None:
        d = ImageDraw.Draw(img)
        d.rectangle((x, y, x + w - 1, y + h - 1), fill=palette.bg_secondary)
        d.line((x, y + h - 1, x + w - 1, y + h - 1), fill=palette.border_default)

        title_f = p.font("sans_bold", 11)
        d.text((x + 12, y + 6), "RADIO LINK", fill=palette.text_tertiary, font=title_f)

        wfb = self._wfb
        state = (wfb.get("state") or "—").upper()
        # State dot color — derived from the link state enum.
        state_color = palette.text_secondary
        if state == "CONNECTED":
            state_color = palette.status_success
        elif state in ("CONNECTING", "DEGRADED", "AUTO_PAIRING", "BINDING"):
            state_color = palette.status_warning
        elif state in ("DISCONNECTED", "UNPAIRED"):
            state_color = palette.status_error
        d.ellipse(
            (x + 90, y + 9, x + 96, y + 15),
            fill=state_color,
        )
        d.text((x + 102, y + 6), state, fill=palette.text_primary, font=title_f)

        ch = wfb.get("channel")
        ch_text = f"ch {ch}" if isinstance(ch, int) and ch > 0 else "ch —"
        d.text(
            (x + w - 70, y + 6), ch_text, fill=palette.text_secondary, font=title_f,
        )

        # First metric row — RSSI + bitrate side by side.
        big_f = p.font("mono_bold", 24)
        small_f = p.font("sans_regular", 10)
        rssi = wfb.get("rssi_dbm")
        rssi_text = (
            f"{int(rssi)}" if isinstance(rssi, (int, float)) else "—"
        )
        rssi_color = _color_for(
            float(rssi) if isinstance(rssi, (int, float)) else None,
            ok=_RSSI_OK_DBM,
            warn=_RSSI_WARN_DBM,
            palette=palette,
            higher_is_better=True,
        )
        d.text((x + 12, y + 26), rssi_text, fill=rssi_color, font=big_f)
        d.text((x + 12, y + 56), "RSSI dBm", fill=palette.text_secondary, font=small_f)

        bitrate = wfb.get("bitrate_kbps")
        bitrate_text = (
            f"{bitrate / 1000:.1f}" if isinstance(bitrate, (int, float)) else "—"
        )
        d.text((x + 100, y + 26), bitrate_text, fill=palette.text_primary, font=big_f)
        d.text(
            (x + 100, y + 56), "Mbps", fill=palette.text_secondary, font=small_f,
        )

        pkts = wfb.get("packets_received")
        lost = wfb.get("packets_lost")
        loss = wfb.get("loss_percent")
        loss_color = _color_for(
            float(loss) if isinstance(loss, (int, float)) else None,
            ok=_LOSS_OK,
            warn=_LOSS_WARN,
            palette=palette,
            higher_is_better=False,
        )
        med_f = p.font("sans_regular", 11)
        pkt_label = f"pkts {pkts if isinstance(pkts, int) else '—'}"
        lost_label = f"lost {lost if isinstance(lost, int) else '—'}"
        loss_label = f"({loss:.1f}%)" if isinstance(loss, (int, float)) else "(—)"
        d.text((x + 200, y + 30), pkt_label, fill=palette.text_primary, font=med_f)
        d.text((x + 200, y + 46), lost_label, fill=palette.text_secondary, font=med_f)
        d.text((x + 280, y + 46), loss_label, fill=loss_color, font=med_f)

        fec_recovered = wfb.get("fec_recovered")
        fec_failed = wfb.get("fec_failed")
        d.text(
            (x + 350, y + 30),
            f"FEC ok {fec_recovered if isinstance(fec_recovered, int) else '—'}",
            fill=palette.text_secondary,
            font=med_f,
        )
        d.text(
            (x + 350, y + 46),
            f"FEC bad {fec_failed if isinstance(fec_failed, int) else '—'}",
            fill=(
                palette.status_error
                if isinstance(fec_failed, int) and fec_failed > 0
                else palette.text_secondary
            ),
            font=med_f,
        )

        # 60 s RSSI sparkline along the bottom of the band.
        if any(v is not None for v in self._rssi_history):
            draw_sparkline(
                img,
                x + 12,
                y + 70,
                w - 24,
                24,
                list(self._rssi_history),
                color=palette.accent_primary,
                fill_below=False,
                y_min=-90.0,
                y_max=-30.0,
            )

    def _draw_dec_band(
        self, img, palette, *, x: int, y: int, w: int, h: int,
    ) -> None:
        d = ImageDraw.Draw(img)
        d.rectangle((x, y, x + w - 1, y + h - 1), fill=palette.bg_primary)
        d.line((x, y + h - 1, x + w - 1, y + h - 1), fill=palette.border_default)

        title_f = p.font("sans_bold", 11)
        d.text((x + 12, y + 6), "DECODER", fill=palette.text_tertiary, font=title_f)
        d.text((x + 240, y + 6), "STREAM", fill=palette.text_tertiary, font=title_f)

        med_f = p.font("sans_regular", 11)
        mono_f = p.font("mono_bold", 14)

        tap = self._tap
        decoder = tap.get("decoder") or "—"
        active = bool(tap.get("active"))
        fps = tap.get("fps")
        fps_color = _color_for(
            float(fps) if isinstance(fps, (int, float)) else None,
            ok=_FPS_OK,
            warn=_FPS_WARN,
            palette=palette,
            higher_is_better=True,
        )
        recording = bool(tap.get("recording"))

        d.text(
            (x + 12, y + 24),
            f"{decoder}",
            fill=palette.text_primary,
            font=med_f,
        )
        fps_text = (
            f"{fps:.1f} fps" if isinstance(fps, (int, float)) else "— fps"
        )
        d.text((x + 12, y + 42), fps_text, fill=fps_color, font=mono_f)
        # Glass-to-glass latency from the SEI marker measurement that
        # LocalVideoTap.stats() writes to /run/ados/lcd-video-tap.json.
        # tap dict carries `latency_ms` when at least one valid sample
        # has been observed; falls back to "— ms" until the first
        # marker arrives. Color-coded: <80 ms green, <150 ms amber.
        latency_ms = tap.get("latency_ms")
        if isinstance(latency_ms, (int, float)):
            latency_text = f"{int(round(latency_ms))} ms"
            if latency_ms <= 80:
                latency_color = palette.status_success
            elif latency_ms <= 150:
                latency_color = palette.status_warning
            else:
                latency_color = palette.status_error
        else:
            latency_text = "— ms"
            latency_color = palette.text_secondary
        d.text((x + 110, y + 42), latency_text, fill=latency_color, font=mono_f)
        if not active:
            d.text(
                (x + 200, y + 46),
                "(tap inactive)",
                fill=palette.text_secondary,
                font=med_f,
            )

        # Stream column — mediamtx state + inbound rate.
        mtx = self._mtx
        ready = bool(mtx.get("ready")) if mtx else False
        ready_text = "ready" if ready else "not-ready"
        ready_color = palette.status_success if ready else palette.status_error
        d.ellipse(
            (x + 240, y + 27, x + 246, y + 33),
            fill=ready_color,
        )
        d.text(
            (x + 252, y + 24),
            f"mediamtx {ready_text}",
            fill=palette.text_primary,
            font=med_f,
        )
        rate = self._mtx_inbound_kbps
        rate_text = (
            f"{rate / 1000:.2f} Mbps in"
            if isinstance(rate, (int, float))
            else "— Mbps in"
        )
        d.text((x + 240, y + 42), rate_text, fill=palette.text_primary, font=mono_f)

        if recording:
            d.ellipse(
                (x + w - 90, y + 27, x + w - 84, y + 33),
                fill=palette.status_error,
            )
            d.text(
                (x + w - 76, y + 24),
                "REC",
                fill=palette.status_error,
                font=med_f,
            )

    def _draw_sys_band(
        self, img, palette, *, x: int, y: int, w: int, h: int,
    ) -> None:
        d = ImageDraw.Draw(img)
        d.rectangle((x, y, x + w - 1, y + h - 1), fill=palette.bg_secondary)

        title_f = p.font("sans_bold", 11)
        d.text((x + 12, y + 6), "SYSTEM", fill=palette.text_tertiary, font=title_f)

        med_f = p.font("mono_bold", 14)
        h_blob = self._health
        cpu = h_blob.get("cpu_percent")
        ram = h_blob.get("memory_percent")
        disk = h_blob.get("disk_percent")
        temp = h_blob.get("temperature")

        ram_color = _color_for(
            float(ram) if isinstance(ram, (int, float)) else None,
            ok=_MEM_WARN - 1,
            warn=_MEM_CRIT - 1,
            palette=palette,
            higher_is_better=False,
        )
        temp_color = _color_for(
            float(temp) if isinstance(temp, (int, float)) else None,
            ok=_TEMP_OK_C,
            warn=_TEMP_WARN_C,
            palette=palette,
            higher_is_better=False,
        )

        x0 = x + 12
        col_w = (w - 24) // 4
        y_val = y + 26
        y_lab = y + 48

        def col(i: int, label: str, value_text: str, color):
            cx = x0 + i * col_w
            d.text((cx, y_val), value_text, fill=color, font=med_f)
            d.text(
                (cx, y_lab),
                label,
                fill=palette.text_secondary,
                font=p.font("sans_regular", 10),
            )

        col(
            0,
            "CPU",
            f"{int(cpu)}%" if isinstance(cpu, (int, float)) else "—",
            palette.text_primary,
        )
        col(
            1,
            "RAM",
            f"{int(ram)}%" if isinstance(ram, (int, float)) else "—",
            ram_color,
        )
        col(
            2,
            "TEMP",
            f"{int(temp)}°C" if isinstance(temp, (int, float)) else "—",
            temp_color,
        )
        col(
            3,
            "DISK",
            f"{int(disk)}%" if isinstance(disk, (int, float)) else "—",
            palette.text_primary,
        )

    # ── touch / hit zones ───────────────────────────────────────

    def hit_zones(self, ctx: PageContext) -> list[HitZone]:
        # Whole-page tap zone — primarily for diagnostic logging today.
        # Future drilldowns (per-band) plug into this id namespace.
        return [HitZone(id="link_stats:body", x=0, y=0, w=PAGE_W, h=PAGE_H)]

    async def on_touch(
        self,
        ctx: PageContext,
        zone: HitZone,
        gesture: TouchGesture,
    ) -> None:
        # No drilldowns yet — just log so we can see operator interaction
        # during bench tests.
        ctx.logger.debug(
            "link_stats_touch", zone=zone.id, kind=gesture.kind,
        )
