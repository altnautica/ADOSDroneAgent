"""Top status bar — 32 px persistent header for every LCD page.

Layout (left to right):

* Hostname (8 px from left, DejaVu Sans Bold 14).
* Role badge: colored dot + uppercase role label.
* Sysmetrics block: CPU + RAM + temperature, threshold-colored.
* Wall clock, mono regular 13, right-aligned 8 px from edge.

The bar is tap-inert — touch handling lives in the tab bar and the
active page. The 32 px height is chosen so a full 480x320 panel can
host a 244 px page region between top (32 px) and bottom (44 px).
"""

from __future__ import annotations

import time
from typing import Any

from PIL import Image, ImageDraw

from ados.services.ui.dashboards.components import primitives as p
from ados.services.ui.dashboards.components.status_dot import draw_dot
from ados.services.ui.theme import Palette

HEIGHT = 32

# Role badge color resolution. Maps role-string to a Palette attribute
# name. Doing the lookup at draw time means a theme flip recolors the
# badge on the next render tick without any change here.
_ROLE_COLOR_ATTR: dict[str, str] = {
    "receiver": "status_success",
    "relay": "accent_primary",
    "direct": "text_secondary",
    "unset": "status_warning",
}


def _role_color(palette: Palette, role: str) -> tuple[int, int, int]:
    attr = _ROLE_COLOR_ATTR.get(role.lower(), "text_secondary")
    return getattr(palette, attr)


def _format_ram(used_mb: int | None, total_mb: int | None) -> str:
    if used_mb is None or total_mb is None or total_mb <= 0:
        return "-"
    used_g = used_mb / 1024.0
    total_g = total_mb / 1024.0
    return f"{used_g:.1f}/{total_g:.0f}G"


def draw(
    image: Image.Image,
    x: int,
    y: int,
    w: int,
    *,
    palette: Palette,
    hostname: str,
    state: dict[str, Any],
    now_str: str | None = None,
) -> None:
    """Paint the top status bar in place.

    ``state`` mirrors the dict shape published by the agent's ground
    station status endpoint. Missing fields fall back to ``-`` so the
    bar always paints even on a fresh boot before the first poll
    completes.

    ``now_str`` overrides the live wall clock; pass it from snapshot
    tests for deterministic output.
    """
    draw_obj = ImageDraw.Draw(image)
    h = HEIGHT
    # Solid background flush against the canvas edge.
    draw_obj.rectangle((x, y, x + w - 1, y + h - 1), fill=palette.bg_primary)

    # Hostname.
    name_font = p.font("sans_bold", 14)
    name = hostname or "groundnode"
    draw_obj.text((x + 8, y + 8), name, fill=palette.text_primary, font=name_font)
    name_w, _ = p.text_size(image, name, name_font)

    # Role badge: dot + label.
    role_block = state.get("role") or {}
    role_current = (role_block.get("current") or "unset").lower()
    role_color = _role_color(palette, role_current)
    role_label = role_current.upper()
    role_x = x + 8 + name_w + 14
    dot_cx = role_x + 6
    dot_cy = y + h // 2
    draw_dot(image, dot_cx, dot_cy, role_color, radius=5)
    role_font = p.font("sans_bold", 12)
    label_x = role_x + 16
    draw_obj.text(
        (label_x, y + 9),
        role_label,
        fill=palette.text_primary,
        font=role_font,
    )
    role_w, _ = p.text_size(image, role_label, role_font)

    # System metrics. CPU + RAM + temperature.
    sys_block = state.get("system") or {}
    cpu = sys_block.get("cpu_pct")
    ram_used = sys_block.get("ram_used_mb")
    ram_total = sys_block.get("ram_total_mb")
    temp = sys_block.get("temp_c")
    label_font = p.font("mono_regular", 11)
    value_font = p.font("mono_regular", 11)

    metrics_x_start = label_x + role_w + 18
    cursor_x = metrics_x_start
    text_y = y + 10

    cpu_color = p.threshold_color(
        cpu, success_at=70, warning_at=85,
        direction="lower_is_better", palette=palette,
    )
    ram_pct = None
    if ram_used is not None and ram_total and ram_total > 0:
        ram_pct = (ram_used / ram_total) * 100.0
    ram_color = p.threshold_color(
        ram_pct, success_at=70, warning_at=85,
        direction="lower_is_better", palette=palette,
    )
    temp_color = p.threshold_color(
        temp, success_at=65, warning_at=75,
        direction="lower_is_better", palette=palette,
    )

    def _emit(label: str, value: str, color: tuple[int, int, int]) -> None:
        nonlocal cursor_x
        draw_obj.text(
            (cursor_x, text_y),
            label,
            fill=palette.text_tertiary,
            font=label_font,
        )
        lw, _ = p.text_size(image, label, label_font)
        cursor_x += lw + 4
        draw_obj.text((cursor_x, text_y), value, fill=color, font=value_font)
        vw, _ = p.text_size(image, value, value_font)
        cursor_x += vw + 12

    cpu_str = f"{int(cpu)}%" if cpu is not None else "-"
    _emit("CPU", cpu_str, cpu_color)
    _emit("RAM", _format_ram(ram_used, ram_total), ram_color)
    temp_str = f"{int(temp)}°C" if temp is not None else "-"
    _emit("T", temp_str, temp_color)

    # Wall clock right-anchored.
    if now_str is None:
        now_str = time.strftime("%H:%M:%S")
    clock_font = p.font("mono_regular", 13)
    clock_w, _ = p.text_size(image, now_str, clock_font)
    draw_obj.text(
        (x + w - 8 - clock_w, y + 9),
        now_str,
        fill=palette.text_secondary,
        font=clock_font,
    )

    # 1 px divider beneath the bar so the page region below sits on a
    # crisp baseline regardless of background contrast.
    draw_obj.line(
        (x, y + h - 1, x + w - 1, y + h - 1),
        fill=palette.border_default,
    )
