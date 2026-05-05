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
from .big_number import draw_big_number
from .bar_meter import draw_bar
from .status_dot import draw_dot
from .tile import draw_tile


# Cap used for the bitrate meter. WFB-ng on RTL8812EU tops out around
# 35 Mbps in our reference rigs; tweak per board if needed.
BITRATE_CAP_MBPS = 35


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
    rssi = link.get("rssi_dbm")
    bitrate = link.get("bitrate_mbps")
    fec_rec = link.get("fec_recovered")
    fec_lost = link.get("fec_lost")
    channel = link.get("channel")

    title_right = f"ch {channel}" if channel is not None else ""
    bx, by, bw, bh = draw_tile(image, x, y, w, h, "Radio link", title_right=title_right)
    draw = ImageDraw.Draw(image)

    # Big RSSI value with threshold color. WFB-ng RSSI usually -40..-90 dBm.
    if rssi is None:
        rssi_text = "— dBm"
        rssi_color = p.TEXT_TERTIARY
    else:
        rssi_text = f"{rssi:.0f}"
        rssi_color = p.threshold_color(
            rssi, success_at=-65, warning_at=-80, direction="higher_is_better"
        )
    draw_big_number(
        image,
        bx,
        by + 4,
        rssi_text,
        color=rssi_color,
        size=32,
        unit="dBm" if rssi is not None else "",
    )

    # Bitrate row — value + segmented bar.
    bitrate_y = by + 50
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
    fec_y = by + 74
    rec_str = f"{fec_rec:,}" if fec_rec is not None else "—"
    lost_str = f"{fec_lost:,}" if fec_lost is not None else "—"
    lost_color = p.STATUS_ERROR if (fec_lost or 0) > 100 else p.TEXT_SECONDARY
    fec_font = p.font("mono_regular", 12)
    draw.text((bx, fec_y), "FEC R", fill=p.TEXT_TERTIARY, font=fec_font)
    draw.text((bx + 38, fec_y), rec_str, fill=p.TEXT_SECONDARY, font=fec_font)
    draw.text((bx + 100, fec_y), "L", fill=p.TEXT_TERTIARY, font=fec_font)
    draw.text((bx + 114, fec_y), lost_str, fill=lost_color, font=fec_font)


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
