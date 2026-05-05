"""Early-life tiles — render when primary data is missing.

The default tiles (RADIO LINK, DRONE, MESH) assume the rig is fully
configured and active. On first boot — no drone paired, no radio
adapter, no mesh peers — those tiles show ``—`` everywhere and
waste pixels. The early-life tiles below replace them with content
the operator actually needs at that stage: pairing instructions,
hardware checklist, wizard URL.

The router in ``groundnode_landscape.py`` decides which tile to
render in each slot. Each function here paints in-place into the
caller's PIL image, same signature as the default tiles.
"""

from __future__ import annotations

from typing import Any

from PIL import Image, ImageDraw

from . import primitives as p
from .qr import render_qr
from .status_dot import draw_dot
from .tile import draw_tile


def _fit_font(
    image: Image.Image,
    text: str,
    family: str,
    max_width: int,
    max_size: int,
    min_size: int = 9,
) -> Any:
    """Return the largest font in [min_size, max_size] that fits ``text`` in ``max_width``."""
    for size in range(max_size, min_size - 1, -1):
        font = p.font(family, size)
        w, _ = p.text_size(image, text, font)
        if w <= max_width:
            return font
    return p.font(family, min_size)


def _truncate_to_width(
    image: Image.Image,
    text: str,
    font: Any,
    max_width: int,
) -> str:
    """Trim ``text`` (with an ellipsis) until it fits in ``max_width``."""
    w, _ = p.text_size(image, text, font)
    if w <= max_width:
        return text
    ellipsis = "…"
    trimmed = text
    while trimmed and p.text_size(image, trimmed + ellipsis, font)[0] > max_width:
        trimmed = trimmed[:-1]
    return (trimmed + ellipsis) if trimmed else ""


# ──────────────────────────────────────────────────────────────────────
# PAIR DRONE — replaces DRONE slot when no drone is paired
# ──────────────────────────────────────────────────────────────────────


def draw_pair_drone_tile(
    image: Image.Image,
    x: int,
    y: int,
    w: int,
    h: int,
    state: dict[str, Any],
    *,
    pulse_phase: int = 0,
) -> None:
    """Pairing-instructions tile.

    Shows a big 6-char pair code, a QR rendering of the pair URL (so
    the operator can scan with their phone), and a "broadcasting"
    pulse dot to confirm the agent is actively beaconing the code.
    """
    cloud = state.get("cloud") or {}
    pairing = state.get("pairing") or {}
    code = (
        pairing.get("code")
        or cloud.get("pair_code")
        or ""
    ).upper()
    setup_url = (
        cloud.get("pair_url")
        or pairing.get("pair_url")
        or "altnautica.com/command"
    )

    bx, by, bw, bh = draw_tile(
        image, x, y, w, h, "Pair drone", title_right="broadcast",
    )
    draw = ImageDraw.Draw(image)

    # Broadcasting pulse — small dot in the title-bar slot. Color
    # alternates between full success-green and dim every other tick
    # so the operator sees life from across the room.
    pulse_color = (
        p.STATUS_SUCCESS if pulse_phase % 2 == 0 else (0x0E, 0x4D, 0x26)
    )
    draw_dot(image, x + w - 70, y + 10, pulse_color, radius=3)

    # Layout: QR sized so the right-hand column is wide enough for
    # the big pair code AND the two hint lines. Cap at 78 px so a
    # ~115 px text column remains.
    qr_size = min(bh - 8, 78)
    qr = render_qr(setup_url + "?pair=" + code if code else setup_url, target_px=qr_size)
    if qr is not None:
        image.paste(qr, (bx, by + 4))
        text_x = bx + qr_size + 10
    else:
        text_x = bx
    text_w = max(0, bw - (text_x - bx))

    # Big pair code — auto-shrink so a 6-char code fits the column.
    if code:
        code_font = _fit_font(image, code, "mono_bold", text_w, 26, min_size=18)
        draw.text((text_x, by + 4), code, fill=p.TEXT_PRIMARY, font=code_font)
    else:
        loading_font = p.font("sans_bold", 13)
        draw.text(
            (text_x, by + 14),
            "waiting…",
            fill=p.TEXT_TERTIARY,
            font=loading_font,
        )

    # Hint lines — measure-and-truncate so a long string never bleeds.
    hint_font = p.font("sans_regular", 11)
    line1 = _truncate_to_width(image, "Open Mission Control →", hint_font, text_w)
    line2 = _truncate_to_width(image, 'Tap "Pair drone"', hint_font, text_w)
    line3 = _truncate_to_width(image, "Enter code above", hint_font, text_w)
    draw.text((text_x, by + 38), line1, fill=p.TEXT_SECONDARY, font=hint_font)
    draw.text((text_x, by + 52), line2, fill=p.TEXT_SECONDARY, font=hint_font)
    draw.text((text_x, by + 66), line3, fill=p.TEXT_SECONDARY, font=hint_font)


# ──────────────────────────────────────────────────────────────────────
# HARDWARE — replaces RADIO LINK slot when no link
# ──────────────────────────────────────────────────────────────────────


def draw_hardware_tile(
    image: Image.Image,
    x: int,
    y: int,
    w: int,
    h: int,
    state: dict[str, Any],
) -> None:
    """Peripheral checklist for the early-setup phase.

    Walks the ``hardware_check.items`` list (when present) and shows
    the three most critical entries: companion compute, WFB radio
    adapter, mesh dongle. Colored bullets give an at-a-glance read
    on what's plugged in vs missing.
    """
    bx, by, bw, bh = draw_tile(image, x, y, w, h, "Hardware", title_right="checklist")
    draw = ImageDraw.Draw(image)

    hw = state.get("hardware_check") or {}
    items = hw.get("items") or []
    # Index by id for cherry-picking the rows we care about.
    by_id = {it.get("id"): it for it in items if isinstance(it, dict)}

    # Highlight the rows the operator can actually act on at the
    # bench right now.
    rows = [
        ("board", "Companion compute", True),
        ("wfb_radio", "WFB radio adapter", True),
        ("mesh_dongle", "Mesh second dongle", False),
        ("display", "Local display", False),
    ]

    line_font = p.font("sans_bold", 13)
    detail_font = p.font("sans_regular", 11)

    line_y = by + 4
    for item_id, label, required in rows:
        item = by_id.get(item_id) or {}
        state_val = (item.get("state") or "unknown").lower()
        if state_val == "ok":
            dot_color = p.STATUS_SUCCESS
            mark = "✓"
        elif state_val in ("warning", "missing") and required:
            dot_color = p.STATUS_ERROR
            mark = "✗"
        elif state_val in ("warning", "missing"):
            dot_color = p.STATUS_WARNING
            mark = "○"
        else:
            dot_color = p.TEXT_TERTIARY
            mark = "·"

        # Status dot left, label, fix hint right (truncated).
        draw_dot(image, bx + 6, line_y + 8, dot_color, radius=4)
        draw.text((bx + 18, line_y + 1), label, fill=p.TEXT_PRIMARY, font=line_font)
        # Fix hint — only when required AND missing — to nudge the
        # operator toward the right physical action.
        hint_text = ""
        if required and state_val in ("warning", "missing"):
            fix_hint = item.get("fix_hint") or ""
            # Trim a long hint so it fits.
            if len(fix_hint) > 36:
                fix_hint = fix_hint[:33] + "…"
            hint_text = fix_hint
        if hint_text:
            draw.text(
                (bx + 18, line_y + 16),
                hint_text,
                fill=p.STATUS_WARNING,
                font=detail_font,
            )
            line_y += 30
        else:
            line_y += 22


# ──────────────────────────────────────────────────────────────────────
# SETUP WIZARD — replaces MESH slot when wizard not finalized
# ──────────────────────────────────────────────────────────────────────


def draw_setup_wizard_tile(
    image: Image.Image,
    x: int,
    y: int,
    w: int,
    h: int,
    state: dict[str, Any],
) -> None:
    """Setup wizard nudge tile.

    Shows the wizard URL big enough to type from across the bench,
    plus the next-step action and a step-progress chip.
    """
    network = state.get("network") or {}
    completion = state.get("completion_percent")
    next_action = state.get("next_action") or ""

    # Pick the most visible URL the operator can hit. The agent's
    # FastAPI server redirects "/" to the wizard SPA, so dropping the
    # ".html" suffix is safe and saves precious horizontal pixels.
    host = network.get("mdns_host") or "groundnode"
    url = f"http://{host}.local:8080"

    title_right = ""
    if completion is not None:
        title_right = f"{int(completion)}%"
    bx, by, bw, bh = draw_tile(
        image, x, y, w, h, "Setup wizard", title_right=title_right,
    )
    draw = ImageDraw.Draw(image)

    # URL — biggest monospace size that fits the body width.
    url_font = _fit_font(image, url, "mono_bold", bw, 14, min_size=10)
    draw.text((bx, by + 4), url, fill=p.TEXT_PRIMARY, font=url_font)

    # Next action — measure-and-truncate so a long string never bleeds.
    action_font = p.font("sans_bold", 12)
    label = "Next:"
    label_w, _ = p.text_size(image, label, action_font)
    draw.text((bx, by + 26), label, fill=p.TEXT_TERTIARY, font=action_font)
    next_avail = max(0, bw - label_w - 6)
    next_text = _truncate_to_width(
        image,
        next_action or "open the URL above",
        action_font,
        next_avail,
    )
    draw.text(
        (bx + label_w + 6, by + 26),
        next_text,
        fill=p.TEXT_SECONDARY,
        font=action_font,
    )

    # Tiny "from any device on this LAN" hint, also safety-truncated.
    hint_font = p.font("sans_regular", 10)
    hint = _truncate_to_width(
        image, "from any browser on this LAN", hint_font, bw,
    )
    draw.text((bx, by + 50), hint, fill=p.TEXT_TERTIARY, font=hint_font)
