"""The four content tiles that fill the dashboard's 2x2 grid.

Each function takes ``image, x, y, w, h, state`` and paints a single
tile in-place. Tile bodies are deliberately compact: ~50 LOC each so
the layout decisions stay legible without scrolling.

Tile order in the grid (top-left → bottom-right):

  RADIO LINK   |   DRONE
  ─────────────┼──────────
  MESH         |   UPLINK / CLOUD
"""

from __future__ import annotations

from typing import Any

from PIL import Image, ImageDraw

from . import primitives as p
from .bar_meter import draw_bar
from .big_number import draw_big_number
from .status_dot import draw_dot
from .tile import draw_tile

# Cap used for the bitrate meter. WFB-ng on RTL8812EU tops out around
# 35 Mbps in our reference rigs; tweak per board if needed.
BITRATE_CAP_MBPS = 35


# Topology badge palette. The four-char chip in the top-right of the
# RADIO LINK tile signals the radio's power-supply path. Host-VBUS is
# the brownout-prone default; powered hub and external 5 V are the
# fixes operators are expected to switch to as they scale TX power.
_TOPOLOGY_BADGES: dict[str, tuple[str, tuple[int, int, int]]] = {
    "host_vbus": ("VBUS", p.BORDER_STRONG),
    "powered_hub": ("HUB", p.ACCENT_PRIMARY),
    "external_5v": ("EXT", p.STATUS_SUCCESS),
}


# Threshold past which host-VBUS topology starts to brown out the
# RTL8812EU on a Pi 4B / Cubie A7Z bus. Mirrors the soft cap in
# ``WfbConfig`` so the dashboard warns before the radio dies.
_BROWNOUT_TX_DBM_THRESHOLD = 12


def _draw_topology_badge(
    image: Image.Image,
    x: int,
    y: int,
    topology: str | None,
) -> None:
    """Paint the four-char topology chip flush to ``(x, y)``'s top-right.

    ``x`` is the chip's right edge, ``y`` is its top edge. The chip is
    sized to fit comfortably alongside the channel text in the tile
    title bar without colliding with it.
    """
    label, fill = _TOPOLOGY_BADGES.get(
        (topology or "").lower(),
        _TOPOLOGY_BADGES["host_vbus"],
    )
    draw = ImageDraw.Draw(image)
    badge_font = p.font("sans_bold", 9)
    text_w, text_h = p.text_size(image, label, badge_font)
    pad_x, pad_y = 4, 1
    chip_w = text_w + pad_x * 2
    chip_h = text_h + pad_y * 2
    chip_x0 = x - chip_w
    chip_y0 = y
    # Rounded rectangle was added in Pillow 8.2; fall back to plain
    # rectangle on older builds so the chip still shows up.
    rounded = getattr(draw, "rounded_rectangle", None)
    if callable(rounded):
        rounded(
            (chip_x0, chip_y0, chip_x0 + chip_w - 1, chip_y0 + chip_h - 1),
            radius=2,
            fill=fill,
        )
    else:
        draw.rectangle(
            (chip_x0, chip_y0, chip_x0 + chip_w - 1, chip_y0 + chip_h - 1),
            fill=fill,
        )
    draw.text(
        (chip_x0 + pad_x, chip_y0 + pad_y - 1),
        label,
        fill=p.TEXT_PRIMARY,
        font=badge_font,
    )


# ──────────────────────────────────────────────────────────────────────
# Tile A — RADIO LINK
# ──────────────────────────────────────────────────────────────────────


def draw_radio_link_tile(
    image: Image.Image,
    x: int,
    y: int,
    w: int,
    h: int,
    state: dict[str, Any],
) -> None:
    link = state.get("link") or {}
    radio = state.get("radio") or {}
    rssi = link.get("rssi_dbm")
    bitrate = link.get("bitrate_mbps")
    fec_rec = link.get("fec_recovered")
    fec_lost = link.get("fec_lost")
    channel = link.get("channel")
    tx_power_dbm = link.get("tx_power_dbm")
    topology = (radio.get("topology") or "host_vbus").lower()

    title_right = f"ch {channel}" if channel is not None else ""
    bx, by, bw, bh = draw_tile(image, x, y, w, h, "Radio link", title_right=title_right)
    draw = ImageDraw.Draw(image)

    # Topology chip in the title-bar top-right. The tile() helper
    # writes ``title_right`` (channel) starting from
    # ``x + w - 8 - text_width``; we place the chip just to the LEFT
    # of where that text starts so the two never overlap. When the
    # channel string is empty, the chip floats flush to the right
    # edge instead.
    chip_anchor_x = x + w - 8
    if title_right:
        right_text_w, _ = p.text_size(image, title_right, p.font("mono_regular", 11))
        chip_anchor_x = x + w - 8 - right_text_w - 4
    _draw_topology_badge(image, chip_anchor_x, y + 3, topology)

    # Big RSSI value with threshold color. WFB-ng RSSI usually -40..-90 dBm.
    if rssi is None:
        rssi_text = "— dBm"
        rssi_color = p.TEXT_TERTIARY
    else:
        rssi_text = f"{rssi:.0f}"
        rssi_color = p.threshold_color(
            rssi, success_at=-55, warning_at=-75, direction="higher_is_better"
        )
    draw_big_number(
        image,
        bx,
        by + 2,
        rssi_text,
        color=rssi_color,
        size=30,
        unit="dBm" if rssi is not None else "",
    )

    # TX power line — sits between the RSSI headline and the bitrate
    # row. Dim secondary text so it reads as metadata, not a main
    # KPI. The brownout pill below escalates this when relevant.
    tx_y = by + 36
    tx_font = p.font("mono_regular", 11)
    if tx_power_dbm is None:
        tx_text = "TX -- dBm"
    else:
        tx_text = f"TX {int(tx_power_dbm)} dBm"
    draw.text((bx, tx_y), tx_text, fill=p.TEXT_SECONDARY, font=tx_font)

    # Bitrate row — value + segmented bar.
    bitrate_y = by + 52
    if bitrate is None:
        draw.text(
            (bx, bitrate_y),
            "— Mbps",
            fill=p.TEXT_TERTIARY,
            font=p.font("mono_regular", 14),
        )
    else:
        draw.text(
            (bx, bitrate_y),
            f"{bitrate:.0f} Mbps",
            fill=p.TEXT_PRIMARY,
            font=p.font("mono_bold", 14),
        )
        # Bar to the right of the value.
        bar_x = bx + 90
        bar_w = bw - 90
        draw_bar(
            image,
            bar_x,
            bitrate_y + 4,
            bar_w,
            8,
            (bitrate / BITRATE_CAP_MBPS) if BITRATE_CAP_MBPS else None,
            segments=6,
            fill_color=p.STATUS_SUCCESS,
        )

    # FEC counts.
    fec_y = by + 72
    rec_str = f"{fec_rec:,}" if fec_rec is not None else "—"
    lost_str = f"{fec_lost:,}" if fec_lost is not None else "—"
    lost_color = p.STATUS_ERROR if (fec_lost or 0) > 100 else p.TEXT_SECONDARY
    fec_font = p.font("mono_regular", 12)
    draw.text((bx, fec_y), "FEC R", fill=p.TEXT_TERTIARY, font=fec_font)
    draw.text((bx + 38, fec_y), rec_str, fill=p.TEXT_SECONDARY, font=fec_font)
    draw.text((bx + 100, fec_y), "L", fill=p.TEXT_TERTIARY, font=fec_font)
    draw.text((bx + 114, fec_y), lost_str, fill=lost_color, font=fec_font)

    # Brownout warning pill at the bottom edge of the body box.
    # Only relevant when the operator is on host-VBUS topology AND
    # TX power has been pushed past the safe envelope. Plain ASCII
    # label — emoji glyphs don't render reliably on the Pi 4B
    # framebuffer pipeline.
    brownout = (
        topology == "host_vbus"
        and tx_power_dbm is not None
        and tx_power_dbm > _BROWNOUT_TX_DBM_THRESHOLD
    )
    if brownout:
        pill_label = "BROWNOUT RISK"
        pill_font = p.font("sans_bold", 10)
        pill_text_w, pill_text_h = p.text_size(image, pill_label, pill_font)
        pill_h = pill_text_h + 4
        pill_y0 = by + bh - pill_h
        rounded = getattr(draw, "rounded_rectangle", None)
        if callable(rounded):
            rounded(
                (bx, pill_y0, bx + bw - 1, pill_y0 + pill_h - 1),
                radius=2,
                fill=p.STATUS_WARNING,
            )
        else:
            draw.rectangle(
                (bx, pill_y0, bx + bw - 1, pill_y0 + pill_h - 1),
                fill=p.STATUS_WARNING,
            )
        draw.text(
            (bx + (bw - pill_text_w) // 2, pill_y0 + 1),
            pill_label,
            fill=p.BG_PRIMARY,
            font=pill_font,
        )


# ──────────────────────────────────────────────────────────────────────
# Tile B — DRONE
# ──────────────────────────────────────────────────────────────────────


def draw_drone_tile(
    image: Image.Image,
    x: int,
    y: int,
    w: int,
    h: int,
    state: dict[str, Any],
) -> None:
    drone = state.get("drone") or {}
    device_id = drone.get("device_id")
    fc_mode = drone.get("fc_mode")
    battery = drone.get("battery_pct")
    gps_sats = drone.get("gps_sats")

    title_right = device_id[-6:].upper() if device_id else ""
    bx, by, bw, bh = draw_tile(image, x, y, w, h, "Drone", title_right=title_right)
    draw = ImageDraw.Draw(image)

    # Empty state when no drone is paired / sending heartbeats.
    if not device_id and battery is None and fc_mode is None and gps_sats is None:
        empty_font = p.font("sans_bold", 14)
        msg = "NO DRONE PAIRED"
        ew, eh = p.text_size(image, msg, empty_font)
        draw.text(
            (bx + (bw - ew) // 2, by + (bh - eh) // 2 - 6),
            msg,
            fill=p.TEXT_SECONDARY,
            font=empty_font,
        )
        # Pairing code if available
        pairing = state.get("pairing") or {}
        code = pairing.get("code") or ""
        if code:
            code_font = p.font("mono_bold", 12)
            cw, ch = p.text_size(image, f"pair: {code}", code_font)
            draw.text(
                (bx + (bw - cw) // 2, by + (bh - eh) // 2 + 14),
                f"pair: {code}",
                fill=p.TEXT_TERTIARY,
                font=code_font,
            )
        return

    # Mode + arm row.
    armed = (fc_mode or "").upper() == "ARMED" or drone.get("armed") is True
    arm_label = "ARMED" if armed else "DISARMED"
    arm_color = p.STATUS_SUCCESS if armed else p.TEXT_SECONDARY
    arm_font = p.font("sans_bold", 14)
    draw.text((bx, by + 4), arm_label, fill=arm_color, font=arm_font)
    if fc_mode:
        mode_font = p.font("mono_bold", 16)
        mode_text = (fc_mode[:6]).upper()
        mw, _ = p.text_size(image, mode_text, mode_font)
        draw.text(
            (bx + bw - mw, by + 2),
            mode_text,
            fill=p.TEXT_PRIMARY,
            font=mode_font,
        )

    # Battery (big) + GPS sat count (right).
    if battery is not None:
        bat_color = p.threshold_color(
            battery, success_at=50, warning_at=20, direction="higher_is_better"
        )
        draw_big_number(
            image, bx, by + 32, f"{int(battery)}", color=bat_color, size=28, unit="%",
        )
    else:
        draw.text(
            (bx, by + 38),
            "BAT —",
            fill=p.TEXT_TERTIARY,
            font=p.font("mono_regular", 14),
        )

    if gps_sats is not None:
        sat_font = p.font("mono_bold", 14)
        sat_marker = "✓" if gps_sats >= 6 else "⚠"
        sat_color = p.STATUS_SUCCESS if gps_sats >= 6 else p.STATUS_WARNING
        sat_text = f"GPS {gps_sats} {sat_marker}"
        sw, _ = p.text_size(image, sat_text, sat_font)
        draw.text(
            (bx + bw - sw, by + 50),
            sat_text,
            fill=sat_color,
            font=sat_font,
        )


# ──────────────────────────────────────────────────────────────────────
# Tile C — MESH
# ──────────────────────────────────────────────────────────────────────


def draw_mesh_tile(
    image: Image.Image,
    x: int,
    y: int,
    w: int,
    h: int,
    state: dict[str, Any],
) -> None:
    role_block = state.get("role") or {}
    mesh_block = state.get("mesh") or {}

    role = (role_block.get("current") or "").lower()
    mesh_capable = bool(role_block.get("mesh_capable"))

    title_right = role or ""
    bx, by, bw, bh = draw_tile(image, x, y, w, h, "Mesh", title_right=title_right)
    draw = ImageDraw.Draw(image)

    # When this node is in 'direct' role and mesh isn't capable, the
    # tile shows N/A — clearer than a row of empty dashes.
    if role == "direct" and not mesh_capable:
        msg = "MESH N/A"
        msg_font = p.font("sans_bold", 14)
        mw, mh = p.text_size(image, msg, msg_font)
        draw.text(
            (bx + (bw - mw) // 2, by + (bh - mh) // 2 - 4),
            msg,
            fill=p.TEXT_TERTIARY,
            font=msg_font,
        )
        sub = "this node is in direct role"
        sub_font = p.font("sans_regular", 11)
        sw, _ = p.text_size(image, sub, sub_font)
        draw.text(
            (bx + (bw - sw) // 2, by + (bh - mh) // 2 + 14),
            sub,
            fill=p.TEXT_TERTIARY,
            font=sub_font,
        )
        return

    up = bool(mesh_block.get("up"))
    partition = bool(mesh_block.get("partition"))
    peer_count = mesh_block.get("peer_count") or 0
    selected_gateway = mesh_block.get("selected_gateway")
    mesh_id = mesh_block.get("mesh_id") or ""

    if not up:
        dot_color = p.TEXT_TERTIARY
        status_label = "down"
    elif partition:
        dot_color = p.STATUS_WARNING
        status_label = f"partitioned · {peer_count} peers"
    else:
        dot_color = p.STATUS_SUCCESS
        status_label = f"up · {peer_count} peers"

    # Status row: dot + label.
    draw_dot(image, bx + 7, by + 14, dot_color, radius=6)
    status_font = p.font("sans_bold", 14)
    draw.text((bx + 22, by + 6), status_label, fill=p.TEXT_PRIMARY, font=status_font)

    # Gateway row.
    detail_font = p.font("sans_regular", 12)
    if selected_gateway:
        draw.text(
            (bx, by + 38),
            f"gw: {selected_gateway}",
            fill=p.TEXT_SECONDARY,
            font=detail_font,
        )
    else:
        draw.text(
            (bx, by + 38),
            "gw: —",
            fill=p.TEXT_TERTIARY,
            font=detail_font,
        )

    # Mesh id (full last 6).
    id_font = p.font("mono_regular", 12)
    if mesh_id:
        draw.text(
            (bx, by + 60),
            f"id: {mesh_id[-6:].upper()}",
            fill=p.TEXT_TERTIARY,
            font=id_font,
        )


# ──────────────────────────────────────────────────────────────────────
# Tile D — UPLINK / CLOUD
# ──────────────────────────────────────────────────────────────────────


def draw_uplink_tile(
    image: Image.Image,
    x: int,
    y: int,
    w: int,
    h: int,
    state: dict[str, Any],
) -> None:
    network = state.get("network") or {}
    cloud = state.get("cloud") or {}

    uplink_type = (network.get("uplink_type") or "none").lower()
    uplink_reachable = bool(network.get("uplink_reachable"))
    latency_ms = cloud.get("latency_ms")
    paired = bool(cloud.get("paired"))
    pair_code = cloud.get("pair_code") or state.get("pairing", {}).get("code") or ""

    if latency_ms is not None:
        title_right = f"{int(latency_ms)} ms"
    elif not uplink_reachable:
        title_right = ""
    else:
        title_right = ""
    bx, by, bw, bh = draw_tile(
        image, x, y, w, h, "Uplink / Cloud", title_right=title_right
    )
    draw = ImageDraw.Draw(image)

    # Uplink status row.
    if uplink_type == "none" or not uplink_reachable:
        dot_color = p.STATUS_ERROR if uplink_type == "none" else p.STATUS_WARNING
        uplink_label = uplink_type if uplink_type != "none" else "OFFLINE"
    else:
        dot_color = p.STATUS_SUCCESS
        uplink_label = uplink_type
    draw_dot(image, bx + 7, by + 14, dot_color, radius=6)
    label_font = p.font("sans_bold", 14)
    draw.text((bx + 22, by + 6), uplink_label, fill=p.TEXT_PRIMARY, font=label_font)

    # Mission Control pair status.
    mc_label = "Mission Control"
    mc_font = p.font("sans_regular", 12)
    draw.text((bx, by + 38), mc_label, fill=p.TEXT_SECONDARY, font=mc_font)

    if paired:
        ok_font = p.font("sans_bold", 13)
        text = "✓ paired"
        draw.text((bx + 110, by + 38), text, fill=p.STATUS_SUCCESS, font=ok_font)
    elif pair_code:
        # Big pair code so the operator can read it from across the bench.
        code_font = p.font("mono_bold", 22)
        cw, _ = p.text_size(image, pair_code, code_font)
        draw.text(
            (bx + (bw - cw) // 2, by + 56),
            pair_code,
            fill=p.TEXT_PRIMARY,
            font=code_font,
        )
    else:
        draw.text((bx + 110, by + 38), "—", fill=p.TEXT_TERTIARY, font=mc_font)
