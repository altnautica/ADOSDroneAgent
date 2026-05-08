"""Bottom-of-dashboard system metrics bar.

28 px tall, full-width band that pins the dashboard. Contents
left to right: CPU+sparkline, RAM, temp+sparkline, uptime, version.
Threshold colors highlight when CPU > 80%, RAM > 80%, temp > 75 C.

The CPU and temp values get a 30x14 sparkline next to the number
showing the last 60 seconds of history — same data, but the trend
catches the eye faster than a rising number alone.
"""

from __future__ import annotations

from typing import Any

from PIL import Image, ImageDraw

from . import primitives as p
from .sparkline import draw_sparkline, history, push

FOOTER_HEIGHT = 28
SPARK_W = 30
SPARK_H = 14


def _format_uptime(seconds: int | None) -> str:
    if seconds is None:
        return "—"
    seconds = int(seconds)
    if seconds < 3600:
        # H:MM:SS
        h, rem = divmod(seconds, 3600)
        m, s = divmod(rem, 60)
        return f"{h:01d}:{m:02d}:{s:02d}"
    if seconds < 86400:
        h, rem = divmod(seconds, 3600)
        m, _ = divmod(rem, 60)
        return f"{h}h {m:02d}m"
    d, rem = divmod(seconds, 86400)
    h, _ = divmod(rem, 3600)
    return f"{d}d {h:02d}h"


def _format_ram(used_mb: int | None, total_mb: int | None) -> str:
    if used_mb is None or total_mb is None or total_mb <= 0:
        return "—"
    used_g = used_mb / 1024.0
    total_g = total_mb / 1024.0
    return f"{used_g:.1f}/{total_g:.0f}G"


def draw_footer(
    image: Image.Image,
    x: int,
    y: int,
    w: int,
    *,
    state: dict[str, Any],
) -> None:
    """Paint the bottom system bar in-place.

    Reads ``state['system']`` for CPU / RAM / temp / uptime /
    agent_version. All values fall back to a muted ``—`` when None.
    """
    draw = ImageDraw.Draw(image)
    h = FOOTER_HEIGHT
    draw.rectangle((x, y, x + w - 1, y + h - 1), fill=p.BG_PRIMARY)
    # 1 px divider above the bar.
    draw.line((x, y, x + w - 1, y), fill=p.BORDER_DEFAULT)

    sys_block = state.get("system") or {}
    cpu = sys_block.get("cpu_pct")
    ram_used = sys_block.get("ram_used_mb")
    ram_total = sys_block.get("ram_total_mb")
    temp = sys_block.get("temp_c")
    uptime = sys_block.get("uptime_seconds")
    version = sys_block.get("agent_version") or "—"

    label_font = p.font("mono_regular", 11)
    value_font = p.font("mono_bold", 12)

    cpu_color = p.threshold_color(cpu, success_at=70, warning_at=85, direction="lower_is_better")
    ram_pct = None
    if ram_used is not None and ram_total and ram_total > 0:
        ram_pct = (ram_used / ram_total) * 100.0
    ram_color = p.threshold_color(ram_pct, success_at=70, warning_at=85, direction="lower_is_better")
    temp_color = p.threshold_color(temp, success_at=65, warning_at=75, direction="lower_is_better")

    # Compose the row as left-anchored chunks. The version sticks
    # right; everything else flows from the left.
    cursor_x = x + 8
    text_y = y + 7

    def _emit(label: str, value: str, color: tuple[int, int, int]) -> None:
        nonlocal cursor_x
        draw.text((cursor_x, text_y + 1), label, fill=p.TEXT_TERTIARY, font=label_font)
        lw, _ = p.text_size(image, label, label_font)
        cursor_x += lw + 4
        draw.text((cursor_x, text_y), value, fill=color, font=value_font)
        vw, _ = p.text_size(image, value, value_font)
        cursor_x += vw + 16

    # Push current samples into the rolling history before drawing.
    # Each render tick adds one sample; sparklines reflect the last
    # 60 ticks (= 60 s at the standard 1 Hz render cadence).
    push("cpu", float(cpu) if cpu is not None else None)
    push("temp", float(temp) if temp is not None else None)

    cpu_str = f"{int(cpu)}%" if cpu is not None else "—"
    _emit("CPU", cpu_str, cpu_color)
    # CPU sparkline pinned to 0-100 so flat-line at 5% reads small,
    # not full-tile.
    spark_y = y + (h - SPARK_H) // 2
    draw_sparkline(
        image,
        cursor_x - 12,
        spark_y,
        SPARK_W,
        SPARK_H,
        history("cpu"),
        color=cpu_color,
        y_min=0,
        y_max=100,
    )
    cursor_x += SPARK_W + 8

    _emit("RAM", _format_ram(ram_used, ram_total), ram_color)

    temp_str = f"{int(temp)}°C" if temp is not None else "—"
    _emit("T", temp_str, temp_color)
    # Temp sparkline auto-scaled to the observed range so a 1°C
    # climb registers visibly. No fixed cap.
    draw_sparkline(
        image,
        cursor_x - 12,
        spark_y,
        SPARK_W,
        SPARK_H,
        history("temp"),
        color=temp_color,
    )
    cursor_x += SPARK_W + 8

    _emit("UP", _format_uptime(uptime), p.TEXT_SECONDARY)

    # Version pinned right.
    version_str = version if version.startswith("v") else f"v{version}"
    vw, _ = p.text_size(image, version_str, value_font)
    draw.text(
        (x + w - 8 - vw, text_y),
        version_str,
        fill=p.TEXT_TERTIARY,
        font=value_font,
    )
