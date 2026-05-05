"""Top-of-dashboard status bar.

32 px tall, full-width band that anchors the dashboard. Contents
left to right:

* Hostname (Space-Grotesk-style display, but DejaVu Sans Bold here)
* Role badge — colored dot + role label (RECEIVER / RELAY / DIRECT / UNSET)
* Mesh ID short hash (only when mesh-capable + has an id)
* Live wall-clock HH:MM:SS

The bar's job is "where am I, what is my role, is the clock right" —
the anchor info that helps the operator orient before drilling into
the tile data below.
"""

from __future__ import annotations

import time
from typing import Any

from PIL import Image, ImageDraw

from . import primitives as p
from .status_dot import draw_dot


HEADER_HEIGHT = 32


_ROLE_COLORS: dict[str, tuple[int, int, int]] = {
    "receiver": p.STATUS_SUCCESS,
    "relay": p.ACCENT_PRIMARY,
    "direct": p.TEXT_SECONDARY,
    "unset": p.STATUS_WARNING,
}


def draw_header(
    image: Image.Image,
    x: int,
    y: int,
    w: int,
    *,
    hostname: str,
    state: dict[str, Any],
    now_str: str | None = None,
) -> None:
    """Paint the dashboard's top status bar in-place.

    ``state`` is the same dict the OLED screens consume — we read
    ``role.current``, ``role.mesh_capable``, ``mesh.mesh_id``.
    ``now_str`` overrides the wall-clock string (for snapshot tests
    where we want a deterministic clock).
    """
    draw = ImageDraw.Draw(image)
    h = HEADER_HEIGHT
    # Background sits flush against the canvas — a true-black band.
    draw.rectangle((x, y, x + w - 1, y + h - 1), fill=p.BG_PRIMARY)

    # Hostname on the far left.
    name_font = p.font("sans_bold", 16)
    name = hostname or "groundnode"
    draw.text((x + 8, y + 7), name, fill=p.TEXT_PRIMARY, font=name_font)
    name_w, _ = p.text_size(image, name, name_font)

    # Role badge a bit to the right of the hostname.
    role_block = state.get("role") or {}
    role_current = (role_block.get("current") or "unset").lower()
    role_color = _ROLE_COLORS.get(role_current, p.TEXT_SECONDARY)
    role_label = role_current.upper()

    role_x = x + 8 + name_w + 18
    dot_cx = role_x + 7
    dot_cy = y + h // 2
    draw_dot(image, dot_cx, dot_cy, role_color, radius=6)
    role_font = p.font("sans_bold", 13)
    draw.text((role_x + 18, y + 9), role_label, fill=p.TEXT_PRIMARY, font=role_font)
    role_w, _ = p.text_size(image, role_label, role_font)

    # Mesh ID short suffix (only when present + meaningful).
    mesh_block = state.get("mesh") or {}
    mesh_id = (mesh_block.get("mesh_id") or "")
    if state.get("role", {}).get("mesh_capable") and mesh_id:
        mesh_short = mesh_id[-6:].upper()
        mesh_label = f"mesh: {mesh_short}"
        mesh_font = p.font("mono_regular", 12)
        mesh_w, _ = p.text_size(image, mesh_label, mesh_font)
        # Roughly center between role and clock.
        mesh_x = x + (w // 2) - (mesh_w // 2) + 30
        draw.text((mesh_x, y + 10), mesh_label, fill=p.TEXT_SECONDARY, font=mesh_font)

    # Wall-clock far right.
    if now_str is None:
        now_str = time.strftime("%H:%M:%S")
    clock_font = p.font("mono_regular", 14)
    clock_w, _ = p.text_size(image, now_str, clock_font)
    draw.text(
        (x + w - 8 - clock_w, y + 9),
        now_str,
        fill=p.TEXT_SECONDARY,
        font=clock_font,
    )

    # 1 px divider under the bar.
    draw.line((x, y + h, x + w - 1, y + h), fill=p.BORDER_DEFAULT)
