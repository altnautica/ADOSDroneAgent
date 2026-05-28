"""Channel Hops page — LCD widget for the HopSupervisor history.

Mirrors what ChannelHistoryChart.tsx renders on the GCS but as a PIL
painting on the 480x244 LCD canvas. The hop supervisor (drone profile,
inside ados-wfb) persists its snapshot to /run/ados/hop-supervisor.json
every 5 s; this page reads the file at 1 Hz and renders the recent
hop history as a step-after line + scatter markers colored by trigger
+ outcome.

Visual layout (top to bottom):
1. Header strip (24 px) — title + band + hop count
2. Chart region (~170 px) — X axis time, Y axis channel, step line
   through hops, scatter marker per hop, dashed reference line at the
   current channel
3. Legend strip (30 px) — colored dots for periodic / reactive /
   failed, plus the last-hop summary

Color semantics match the GCS chart so the operator sees the same
encoding on either surface:
- Green (palette.status_success / #22c55e): periodic + ok
- Amber (palette.status_warning / #f59e0b): reactive + ok
- Red   (palette.status_error   / #ef4444): any failed
- Blue  (palette.accent_primary / #3a82ff): current-channel ref line
"""

from __future__ import annotations

import json
import time
from typing import Any, ClassVar

from PIL import Image, ImageDraw

from ados.core.paths import HOP_SUPERVISOR_JSON
from ados.services.ui.dashboards.components import primitives as p
from ados.services.ui.pages.base import HitZone, PageContext
from ados.services.ui.touch.events import TouchGesture

PAGE_W = 480
PAGE_H = 244

# Vertical layout (numbers add up to PAGE_H exactly).
_HEADER_H = 24
_LEGEND_H = 30
_CHART_H = PAGE_H - _HEADER_H - _LEGEND_H  # 190 px

# 1 Hz file read; matches the supervisor's 5 s persist cadence with
# room to spare. Re-reading a small JSON every second is cheap.
_REFRESH_INTERVAL_S = 1.0

# Chart inner margins.
_CHART_LEFT_PAD = 36
_CHART_RIGHT_PAD = 12
_CHART_TOP_PAD = 8
_CHART_BOTTOM_PAD = 22  # leaves room for X-axis ticks

# Y-axis: a small breath above/below the actual data extrema so dots
# don't sit on the axis.
_Y_PAD_CHANNELS = 4

# Maximum hops the supervisor's snapshot caps at; we mirror it here
# to bound the loops.
_MAX_HOPS = 32


def _safe_dict(value: Any) -> dict:
    return value if isinstance(value, dict) else {}


class ChannelHopsPage:
    """LCD page showing the recent channel-hopping history.

    Pure read-only watch surface: no tap drilldowns, no actions.
    State lives in the JSON file written by HopSupervisor — the page
    re-fetches on every refresh and does not maintain its own ring
    buffer.
    """

    id: ClassVar[str] = "channel_hops"
    refresh_hz: ClassVar[float] = 2.0

    def __init__(self) -> None:
        self._hopping: dict[str, Any] = {}
        self._radio_channel: int | None = None
        self._last_refresh_at: float = 0.0

    # ── lifecycle ──────────────────────────────────────────────

    async def on_enter(self, ctx: PageContext) -> None:
        ctx.logger.info("channel_hops_enter")
        await self._refresh(ctx, force=True)

    async def on_leave(self, ctx: PageContext) -> None:
        ctx.logger.info("channel_hops_leave")

    # ── refresh ────────────────────────────────────────────────

    async def _refresh(self, ctx: PageContext, *, force: bool = False) -> None:
        now = time.monotonic()
        if not force and (now - self._last_refresh_at) < _REFRESH_INTERVAL_S:
            return
        self._last_refresh_at = now

        blob = self._read_run_json(str(HOP_SUPERVISOR_JSON))
        if blob:
            self._hopping = blob

        # The current radio channel pairs with the chart's reference
        # line. Try /api/wfb (same shape link_stats.py uses), fall
        # back to the state.link block populated by the OLED service's
        # internal 1 Hz poll. Either gives us the live channel; a
        # stale one is fine for a passive display.
        ch: int | None = None
        if ctx.http is not None:
            try:
                r = await ctx.http.get("/api/wfb", timeout=1.0)
                if r.status_code == 200:
                    body = r.json()
                    if isinstance(body, dict):
                        v = body.get("channel")
                        if isinstance(v, int) and v > 0:
                            ch = v
            except Exception:  # noqa: BLE001
                pass
        if ch is None:
            link = ctx.state.get("link") if hasattr(ctx, "state") else None
            if isinstance(link, dict):
                v = link.get("channel")
                if isinstance(v, int) and v > 0:
                    ch = v
        self._radio_channel = ch

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

        history = self._history()

        # Header strip is the same in both empty + populated states.
        self._draw_header(img, palette, history)

        if not history:
            self._draw_empty(img, palette)
            return img

        # Chart region: axes + reference line + step-after series + dots.
        self._draw_chart(img, palette, history)
        self._draw_legend(img, palette, history)
        return img

    # ── data accessors ──────────────────────────────────────────

    def _history(self) -> list[dict[str, Any]]:
        raw = self._hopping.get("history") if self._hopping else None
        if not isinstance(raw, list):
            return []
        out: list[dict[str, Any]] = []
        for entry in raw[-_MAX_HOPS:]:
            if not isinstance(entry, dict):
                continue
            if not all(k in entry for k in ("at", "from", "to", "ok")):
                continue
            out.append(entry)
        return out

    def _band(self) -> str:
        v = self._hopping.get("band") if self._hopping else None
        return str(v) if isinstance(v, str) and v else "—"

    # ── band drawers ───────────────────────────────────────────

    def _draw_header(self, img, palette, history: list[dict[str, Any]]) -> None:
        d = ImageDraw.Draw(img)
        d.rectangle((0, 0, PAGE_W - 1, _HEADER_H - 1), fill=palette.bg_secondary)
        d.line(
            (0, _HEADER_H - 1, PAGE_W - 1, _HEADER_H - 1),
            fill=palette.border_default,
        )
        title_f = p.font("sans_bold", 11)
        d.text((12, 6), "CHANNEL HOPS", fill=palette.text_tertiary, font=title_f)
        right = f"{self._band()} · {len(history)} hops"
        # Right-justify approximately; small font so width is small.
        d.text((PAGE_W - 150, 6), right, fill=palette.text_secondary, font=title_f)

    def _draw_empty(self, img, palette) -> None:
        d = ImageDraw.Draw(img)
        big_f = p.font("sans_bold", 14)
        small_f = p.font("sans_regular", 11)
        msg = "No hops yet"
        sub = (
            f"current channel {self._radio_channel}"
            if self._radio_channel is not None
            else "supervisor is armed"
        )
        # Roughly center vertically inside the chart band.
        d.text(
            (PAGE_W // 2 - 50, _HEADER_H + _CHART_H // 2 - 16),
            msg,
            fill=palette.text_primary,
            font=big_f,
        )
        d.text(
            (PAGE_W // 2 - 80, _HEADER_H + _CHART_H // 2 + 4),
            sub,
            fill=palette.text_tertiary,
            font=small_f,
        )

    def _draw_chart(
        self, img, palette, history: list[dict[str, Any]],
    ) -> None:
        d = ImageDraw.Draw(img)
        chart_x0 = _CHART_LEFT_PAD
        chart_y0 = _HEADER_H + _CHART_TOP_PAD
        chart_x1 = PAGE_W - _CHART_RIGHT_PAD
        chart_y1 = _HEADER_H + _CHART_H - _CHART_BOTTOM_PAD
        chart_w = chart_x1 - chart_x0
        chart_h = chart_y1 - chart_y0

        # Y-axis domain: span the actual hop channels + the live ref
        # channel, with a small pad either side.
        ys = [int(e["to"]) for e in history]
        if self._radio_channel is not None:
            ys.append(self._radio_channel)
        y_min = max(1, min(ys) - _Y_PAD_CHANNELS)
        y_max = min(165, max(ys) + _Y_PAD_CHANNELS)
        if y_max <= y_min:
            y_max = y_min + 1

        # X-axis domain: seconds-from-start of the oldest entry.
        t0 = float(history[0]["at"])
        t_last = float(history[-1]["at"])
        x_span = max(1.0, t_last - t0)

        def to_px(at: float, ch: float) -> tuple[int, int]:
            tx = (at - t0) / x_span
            ty = (ch - y_min) / (y_max - y_min)
            px = chart_x0 + int(tx * chart_w)
            py = chart_y1 - int(ty * chart_h)
            return px, py

        # Background.
        d.rectangle(
            (chart_x0 - 1, chart_y0 - 1, chart_x1 + 1, chart_y1 + 1),
            outline=palette.border_default,
            fill=palette.bg_secondary,
        )

        # Y-axis tick labels at min, mid, max.
        axis_f = p.font("mono_regular", 9)
        for ch in (y_min, (y_min + y_max) // 2, y_max):
            _, py = to_px(t0, ch)
            d.text(
                (4, py - 6), str(ch), fill=palette.text_tertiary, font=axis_f,
            )
            d.line(
                (chart_x0 - 2, py, chart_x0, py),
                fill=palette.border_default,
            )

        # X-axis tick labels: oldest (left), midpoint, newest (right).
        now_s = time.time()
        for t_at, anchor_x in (
            (t0, chart_x0),
            ((t0 + t_last) / 2.0, (chart_x0 + chart_x1) // 2),
            (t_last, chart_x1 - 30),
        ):
            delta = int(now_s - t_at)
            label = "now" if delta <= 1 else f"-{delta}s"
            d.text(
                (anchor_x, chart_y1 + 4),
                label,
                fill=palette.text_tertiary,
                font=axis_f,
            )

        # Dashed reference line at the current channel (if known and
        # inside the visible range).
        if self._radio_channel is not None and y_min <= self._radio_channel <= y_max:
            _, ref_py = to_px(t0, float(self._radio_channel))
            self._draw_dashed_hline(
                d, chart_x0, ref_py, chart_x1, palette.accent_primary,
            )

        # Step-after path through (from -> to) pairs.
        # Build a series of (px, py) pairs by walking through history;
        # each hop emits a horizontal segment at the "from" channel
        # and a vertical jump to the "to" channel.
        line_color = palette.text_secondary
        prev_px: int | None = None
        prev_py: int | None = None
        for i, entry in enumerate(history):
            at = float(entry["at"])
            from_ch = float(entry["from"])
            to_ch = float(entry["to"])
            from_px, from_py = to_px(at, from_ch)
            _to_px_, to_py = to_px(at, to_ch)
            # Horizontal extension from the previous point to this hop's
            # x position at the previous channel.
            if prev_px is not None and prev_py is not None:
                d.line(
                    (prev_px, prev_py, from_px, prev_py),
                    fill=line_color,
                    width=1,
                )
                # Vertical from previous channel to this hop's "from"
                # channel (usually equal — this is a continuity check).
                if prev_py != from_py:
                    d.line(
                        (from_px, prev_py, from_px, from_py),
                        fill=line_color,
                        width=1,
                    )
            # Vertical jump at this hop to the new channel.
            d.line(
                (from_px, from_py, from_px, to_py),
                fill=line_color,
                width=1,
            )
            prev_px, prev_py = from_px, to_py

        # Extend the last step to the right edge so the operator sees
        # "we're on this channel right now."
        if prev_px is not None and prev_py is not None and prev_px < chart_x1:
            d.line(
                (prev_px, prev_py, chart_x1, prev_py),
                fill=line_color,
                width=1,
            )

        # Scatter markers per hop, colored by trigger + outcome.
        for entry in history:
            at = float(entry["at"])
            to_ch = float(entry["to"])
            ok = bool(entry.get("ok", False))
            trigger = str(entry.get("trigger", "periodic"))
            color = self._marker_color(palette, trigger, ok)
            cx, cy = to_px(at, to_ch)
            d.ellipse(
                (cx - 3, cy - 3, cx + 3, cy + 3),
                fill=color,
                outline=palette.bg_primary,
                width=1,
            )

    @staticmethod
    def _marker_color(palette, trigger: str, ok: bool):
        if not ok:
            return palette.status_error
        if trigger == "reactive":
            return palette.status_warning
        return palette.status_success

    @staticmethod
    def _draw_dashed_hline(d, x0: int, y: int, x1: int, color) -> None:
        seg = 4
        gap = 3
        x = x0
        while x < x1:
            d.line((x, y, min(x + seg, x1), y), fill=color, width=1)
            x += seg + gap

    def _draw_legend(
        self, img, palette, history: list[dict[str, Any]],
    ) -> None:
        d = ImageDraw.Draw(img)
        legend_y = _HEADER_H + _CHART_H
        d.rectangle(
            (0, legend_y, PAGE_W - 1, PAGE_H - 1),
            fill=palette.bg_secondary,
        )
        d.line((0, legend_y, PAGE_W - 1, legend_y), fill=palette.border_default)
        f = p.font("sans_regular", 10)

        # Left-side legend: three colored dots + labels.
        items = (
            (palette.status_success, "periodic"),
            (palette.status_warning, "reactive"),
            (palette.status_error, "failed"),
        )
        cx = 14
        cy = legend_y + 14
        for color, label in items:
            d.ellipse((cx - 4, cy - 4, cx + 4, cy + 4), fill=color)
            d.text((cx + 8, cy - 6), label, fill=palette.text_primary, font=f)
            cx += 90

        # Right-side: last-hop summary "last: -56s (40 -> 44)".
        last = history[-1]
        try:
            delta = int(time.time() - float(last["at"]))
        except (KeyError, TypeError, ValueError):
            delta = 0
        ago = f"-{delta}s"
        summary = (
            f"last: {ago} ({int(last['from'])} -> {int(last['to'])})"
        )
        d.text(
            (PAGE_W - 170, cy - 6),
            summary,
            fill=palette.text_secondary,
            font=f,
        )

    # ── input ──────────────────────────────────────────────────

    def hit_zones(self, ctx: PageContext) -> list[HitZone]:
        # Pure read-only surface — no tap drilldowns.
        return []

    async def on_touch(
        self,
        ctx: PageContext,
        zone: HitZone,
        gesture: TouchGesture,
    ) -> None:
        # No hit zones registered; nothing to dispatch.
        return None
