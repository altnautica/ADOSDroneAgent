"""Diagnostics detail page reachable from the More tab.

A read-only system-info drilldown for an operator who wants more than
the top-bar pill exposes. The body has three sections:

1. **System metrics** (CPU% / RAM% / temp / uptime) — same numbers
   the top bar shows but bigger and each paired with a 60 s sparkline
   so an operator can see whether the box is climbing or settling.
2. **Identity** — agent version, board name, device id, primary IP,
   primary MAC. One line per field.
3. **Recent agent logs** — the last 10 lines from
   ``journalctl -u ados-agent`` rendered in DejaVu Sans Mono 9 with
   error/warning lines tinted to match severity. The section is
   scrollable via the touch move bus + kinetic decay so an operator
   can flick through the buffer without leaving the LCD.

Data flows from a single endpoint: ``GET /api/v1/diagnostics``. The
endpoint composes its own response from psutil + the HAL detect helper
+ a one-shot journalctl shell-out. The page caches the parsed payload
and refreshes the system-metrics section at 1 Hz; the log buffer is
fetched once on ``on_enter`` and only re-fetched on a manual refresh
gesture (a swipe-down at the top of the log pane).
"""

from __future__ import annotations

import asyncio
import time
from collections import deque
from typing import Any, ClassVar

from PIL import Image, ImageDraw

from ados.services.ui.dashboards.components import primitives as p
from ados.services.ui.dashboards.components.sparkline import draw_sparkline
from ados.services.ui.touch.events import TouchGesture
from ados.services.ui.touch.kinetic import KineticDecay

from ..base import HitZone, PageContext
from ._common import HEADER_H, draw_header_band

PAGE_W = 480
PAGE_H = 244

# Section heights inside the 480x204 body region (header takes the
# top 40 px). The two text sections are fixed; the log section gets
# the remainder so it can scroll inside its own clipped band.
_METRICS_H = 56
_IDENTITY_H = 60
_LOG_H = PAGE_H - HEADER_H - _METRICS_H - _IDENTITY_H  # ≈ 88 px

_LOG_ROW_H = 12
_LOG_LEFT_PAD = 12
_LOG_TOP_PAD = 4

# Sparkline ring buffer — 60 samples covers 60 s at 1 Hz polling.
_HISTORY_LEN = 60

# Refresh throttling for the system metrics section. The full
# diagnostics endpoint is heavy enough we don't want to hit it on
# every render tick; refresh once per second from inside the render
# loop.
_METRICS_REFRESH_S = 1.0


def _safe_dict(value: Any) -> dict:
    return value if isinstance(value, dict) else {}


def _format_uptime(seconds: float | None) -> str:
    if not isinstance(seconds, (int, float)) or seconds < 0:
        return "--"
    s = int(seconds)
    if s < 60:
        return f"{s}s"
    if s < 3600:
        return f"{s // 60}m {s % 60}s"
    if s < 86400:
        h, rem = divmod(s, 3600)
        return f"{h}h {rem // 60}m"
    d, rem = divmod(s, 86400)
    return f"{d}d {rem // 3600}h"


def _classify_log_level(line: str) -> str:
    """Return ``error`` / ``warning`` / ``info`` for a log line.

    The journalctl ``-o cat`` output strips the level prefix, so we
    fall back to keyword sniffing. Conservative: anything that says
    ``error``/``traceback``/``exception`` is red, ``warn`` is amber,
    everything else is the secondary text tone.
    """
    low = line.lower()
    if (
        "error" in low
        or "traceback" in low
        or "exception" in low
        or "critical" in low
        or "failed" in low
    ):
        return "error"
    if "warn" in low:
        return "warning"
    return "info"


class DiagnosticsDetailPage:
    """The Diagnostics drilldown from the More tab."""

    id: ClassVar[str] = "details.diagnostics"
    refresh_hz: ClassVar[float] = 1.0

    def __init__(self) -> None:
        self._diag: dict[str, Any] = {}
        self._cpu_history: deque[float | None] = deque(maxlen=_HISTORY_LEN)
        self._ram_history: deque[float | None] = deque(maxlen=_HISTORY_LEN)
        self._temp_history: deque[float | None] = deque(maxlen=_HISTORY_LEN)
        self._last_metrics_at: float = 0.0
        self._kinetic = KineticDecay()
        self._move_task: asyncio.Task | None = None
        self._move_active: bool = False
        self._log_offset: int = 0
        self._last_render_ms: int = 0

    # ── lifecycle ──────────────────────────────────────────────

    async def on_enter(self, ctx: PageContext) -> None:
        ctx.logger.info("details_diagnostics_enter")
        await self._refresh(ctx, full=True)
        self._maybe_subscribe_moves(ctx)

    async def on_leave(self, ctx: PageContext) -> None:
        ctx.logger.info("details_diagnostics_leave")
        self._move_active = False
        if self._move_task is not None and not self._move_task.done():
            self._move_task.cancel()
            try:
                await self._move_task
            except (asyncio.CancelledError, Exception):
                pass
        self._move_task = None
        self._kinetic.stop()

    def _maybe_subscribe_moves(self, ctx: PageContext) -> None:
        if ctx.touch_move_bus is None:
            return
        if self._move_task is not None and not self._move_task.done():
            return
        self._move_active = True
        self._move_task = asyncio.create_task(self._consume_moves(ctx))

    async def _consume_moves(self, ctx: PageContext) -> None:
        bus = ctx.touch_move_bus
        if bus is None:
            return
        last_y: int | None = None
        log_top = HEADER_H + _METRICS_H + _IDENTITY_H
        try:
            async for move in bus.subscribe():
                if not self._move_active:
                    break
                # Only react when the pen is over the log pane (LCD
                # global y = 32 + page-local y). The chrome adds 32 px
                # of top bar to every page-local coordinate.
                page_y = move.y_lcd - 32
                if page_y < log_top or page_y >= PAGE_H:
                    last_y = move.y_lcd
                    continue
                if last_y is None:
                    last_y = move.y_lcd
                    continue
                dy = last_y - move.y_lcd
                last_y = move.y_lcd
                if dy:
                    self._kinetic.stop()
                    self._scroll_log_by(dy)
        except asyncio.CancelledError:
            return
        except Exception as exc:  # noqa: BLE001
            ctx.logger.debug(
                "details_diagnostics_move_loop_failed", error=str(exc),
            )

    # ── refresh ────────────────────────────────────────────────

    async def _refresh(self, ctx: PageContext, *, full: bool = False) -> None:
        client = ctx.http
        if client is None:
            return
        now = time.monotonic()
        if not full and (now - self._last_metrics_at) < _METRICS_REFRESH_S:
            return
        try:
            r = await client.get("/api/v1/diagnostics", timeout=2.0)
            if r.status_code == 200:
                blob = r.json() if callable(getattr(r, "json", None)) else {}
                if isinstance(blob, dict):
                    self._diag = blob
                    self._record_metric_history(blob)
                    self._last_metrics_at = now
        except Exception as exc:  # noqa: BLE001
            ctx.logger.debug(
                "details_diagnostics_fetch_failed", error=str(exc),
            )

    def _record_metric_history(self, blob: dict) -> None:
        system = _safe_dict(blob.get("system"))
        cpu = system.get("cpu_percent")
        ram_used = system.get("memory_used_mb")
        ram_total = system.get("memory_total_mb")
        temp = system.get("temp_c")
        if isinstance(cpu, (int, float)):
            self._cpu_history.append(float(cpu))
        else:
            self._cpu_history.append(None)
        if isinstance(ram_used, (int, float)) and isinstance(
            ram_total, (int, float)
        ) and ram_total > 0:
            self._ram_history.append(100.0 * float(ram_used) / float(ram_total))
        else:
            self._ram_history.append(None)
        if isinstance(temp, (int, float)):
            self._temp_history.append(float(temp))
        else:
            self._temp_history.append(None)

    # ── render ─────────────────────────────────────────────────

    async def render(self, ctx: PageContext) -> Image.Image:
        await self._refresh(ctx)
        # Advance kinetic decay before paint.
        now_ms = int(time.monotonic() * 1000)
        if self._last_render_ms == 0:
            dt = 1.0 / max(self.refresh_hz, 1.0)
        else:
            dt = (now_ms - self._last_render_ms) / 1000.0
        self._last_render_ms = now_ms
        if self._kinetic.active:
            self._scroll_log_by(int(self._kinetic.tick(dt)))

        palette = ctx.palette
        img = Image.new("RGB", (PAGE_W, PAGE_H), palette.bg_primary)
        draw_header_band(img, palette, "Diagnostics")
        d = ImageDraw.Draw(img)

        self._render_metrics_section(img, d, palette)
        self._render_identity_section(img, d, palette)
        self._render_log_section(img, d, palette)
        return img

    def _render_metrics_section(
        self,
        img: Image.Image,
        d: ImageDraw.ImageDraw,
        palette,  # type: ignore[no-untyped-def]
    ) -> None:
        section_y = HEADER_H + 4
        system = _safe_dict(self._diag.get("system"))
        agent = _safe_dict(self._diag.get("agent"))

        cpu = system.get("cpu_percent")
        ram_used = system.get("memory_used_mb")
        ram_total = system.get("memory_total_mb")
        temp = system.get("temp_c")
        uptime = agent.get("uptime_seconds")

        def _pct(used: Any, total: Any) -> int | None:
            if (
                isinstance(used, (int, float))
                and isinstance(total, (int, float))
                and total
            ):
                return int(round(100.0 * float(used) / float(total)))
            return None

        ram_pct = _pct(ram_used, ram_total)

        # Layout: 4 columns of width 116, each with a 16 px label + 24
        # px value + 14 px sparkline (10 px tall).
        col_w = (PAGE_W - 16) // 4
        items: list[tuple[str, str, deque[float | None] | None, tuple[int, int, int]]] = []
        items.append((
            "CPU",
            f"{int(cpu)}%" if isinstance(cpu, (int, float)) else "--",
            self._cpu_history,
            palette.accent_primary,
        ))
        items.append((
            "RAM",
            f"{ram_pct}%" if ram_pct is not None else "--",
            self._ram_history,
            palette.accent_primary,
        ))
        items.append((
            "TEMP",
            f"{int(temp)}°" if isinstance(temp, (int, float)) else "--",
            self._temp_history,
            palette.status_warning,
        ))
        items.append((
            "UPTIME",
            _format_uptime(uptime),
            None,
            palette.text_secondary,
        ))

        label_font = p.font("sans_bold", 10)
        value_font = p.font("sans_bold", 16)
        for i, (label, value, history, color) in enumerate(items):
            cx = 8 + i * col_w
            d.text((cx, section_y), label, fill=palette.text_tertiary, font=label_font)
            d.text(
                (cx, section_y + 14),
                value,
                fill=palette.text_primary,
                font=value_font,
            )
            if history is not None and len(history) >= 2:
                draw_sparkline(
                    img,
                    cx,
                    section_y + 38,
                    col_w - 12,
                    14,
                    list(history),
                    color=color,
                    y_min=0,
                    y_max=100 if label != "TEMP" else None,
                )

    def _render_identity_section(
        self,
        img: Image.Image,
        d: ImageDraw.ImageDraw,
        palette,  # type: ignore[no-untyped-def]
    ) -> None:
        agent = _safe_dict(self._diag.get("agent"))
        board = _safe_dict(self._diag.get("board"))
        device = _safe_dict(self._diag.get("device"))
        net = _safe_dict(self._diag.get("network"))

        section_y = HEADER_H + _METRICS_H

        ip = net.get("ip") or "--"
        mac = net.get("mac_eth0") or net.get("mac_wlan0") or "--"
        rows = [
            (
                f"{board.get('name') or '--'}  ·  agent {agent.get('version') or '--'}",
                palette.text_primary,
                p.font("sans_bold", 12),
            ),
            (
                f"id {device.get('device_id') or '--'}",
                palette.text_secondary,
                p.font("mono_regular", 11),
            ),
            (
                f"ip {ip}  ·  mac {mac}",
                palette.text_secondary,
                p.font("mono_regular", 11),
            ),
        ]
        cy = section_y + 4
        for text, color, font in rows:
            d.text((12, cy), text, fill=color, font=font)
            cy += 18

    def _render_log_section(
        self,
        img: Image.Image,
        d: ImageDraw.ImageDraw,
        palette,  # type: ignore[no-untyped-def]
    ) -> None:
        section_y = HEADER_H + _METRICS_H + _IDENTITY_H
        # Divider line above the log pane.
        d.line(
            (0, section_y, PAGE_W - 1, section_y),
            fill=palette.border_default,
        )
        section_label_font = p.font("sans_bold", 10)
        d.text(
            (12, section_y + 2),
            "AGENT LOGS",
            fill=palette.text_tertiary,
            font=section_label_font,
        )

        logs = _safe_dict(self._diag.get("logs"))
        lines = logs.get("agent")
        if not isinstance(lines, list):
            lines = []
        line_font = p.font("mono_regular", 9)
        # Reserve 14 px for the section header so lines start beneath.
        log_band_top = section_y + 16
        log_band_bottom = PAGE_H - 1
        # Clip the log lines to the section band.
        max_visible = max(0, (log_band_bottom - log_band_top) // _LOG_ROW_H)
        # Apply scroll offset (in pixels). The first visible line index
        # advances by offset / row height; remainder pushes the row up
        # for sub-pixel scroll feel.
        first_line = max(0, self._log_offset // _LOG_ROW_H)
        sub_pixel = self._log_offset % _LOG_ROW_H
        for i in range(max_visible + 1):
            idx = first_line + i
            if idx >= len(lines):
                break
            row_y = log_band_top + i * _LOG_ROW_H - sub_pixel
            if row_y >= log_band_bottom:
                break
            line = str(lines[idx])
            level = _classify_log_level(line)
            color = (
                palette.status_error
                if level == "error"
                else palette.status_warning
                if level == "warning"
                else palette.text_secondary
            )
            # Truncate to fit the panel width — rough char budget at
            # mono 9 is ~78 chars, we cut at 80 with an ellipsis.
            text = line if len(line) <= 80 else line[:79] + "…"
            d.text((_LOG_LEFT_PAD, row_y), text, fill=color, font=line_font)

    # ── scroll math ────────────────────────────────────────────

    def _scroll_log_by(self, dy: int) -> None:
        if dy == 0:
            return
        logs = _safe_dict(self._diag.get("logs"))
        lines = logs.get("agent")
        n = len(lines) if isinstance(lines, list) else 0
        max_offset = max(0, n * _LOG_ROW_H - _LOG_H)
        new_off = self._log_offset + dy
        if new_off < 0:
            new_off = 0
        elif new_off > max_offset:
            new_off = max_offset
        self._log_offset = new_off

    # ── hit zones + dispatch ───────────────────────────────────

    def hit_zones(self, ctx: PageContext) -> list[HitZone]:
        zones: list[HitZone] = [HitZone(id="details.back", x=8, y=8, w=40, h=32)]
        # Log pane scroll capture.
        log_top = HEADER_H + _METRICS_H + _IDENTITY_H
        zones.append(
            HitZone(
                id="diagnostics.log_scroll",
                x=0,
                y=log_top,
                w=PAGE_W,
                h=PAGE_H - log_top,
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
        if zone.id == "diagnostics.log_scroll" and gesture.kind == "drag":
            v = gesture.velocity_px_per_s
            if gesture.direction == "down":
                v = -v
            elif gesture.direction != "up":
                return
            self._kinetic.start(v)
            return
