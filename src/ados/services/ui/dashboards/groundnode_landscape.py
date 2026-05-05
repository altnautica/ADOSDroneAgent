"""Single-screen landscape dashboard for the SPI LCD.

480x320 RGB canvas. Header bar (32 px) + 2x2 tile grid (124 px each
row) + footer bar (28 px). All four tiles always render simultaneously
so the operator's at-a-glance scan never misses anything to a
carousel rotation. The layout mirrors the spec at
``~/.claude/plans/i-just-got-a-dapper-ocean.md``.

The renderer is intentionally pure: ``render(state) -> PIL.Image``.
It does no I/O of its own (no HTTP polling, no /sys reads). Caller
hands in the same state dict the OLED carousel screens consume,
plus an optional ``hostname`` (defaults to /etc/hostname read).

This module also has a CLI snapshot mode for visual review on dev
boxes:

    python -m ados.services.ui.dashboards.groundnode_landscape \
        --snapshot /tmp/dash.png

With ``--mock`` it synthesizes a realistic state dict for the
snapshot — useful when developing on the Mac with no agent running.
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any

from PIL import Image

from .components import primitives as p
from .components.footer_bar import FOOTER_HEIGHT, draw_footer
from .components.header_bar import HEADER_HEIGHT, draw_header
from .components.tiles import (
    draw_drone_tile,
    draw_mesh_tile,
    draw_radio_link_tile,
    draw_uplink_tile,
)


CANVAS_W = 480
CANVAS_H = 320

OUTER_MARGIN = 8
TILE_GAP = 8


def _read_hostname() -> str:
    try:
        return Path("/etc/hostname").read_text().strip()
    except OSError:
        return "groundnode"


def render(
    state: dict[str, Any],
    *,
    hostname: str | None = None,
    now_str: str | None = None,
) -> Image.Image:
    """Paint the full dashboard and return a 480x320 RGB image.

    ``state`` is the same dict published by
    ``GET /api/v1/ground-station/status``. ``hostname`` overrides the
    /etc/hostname read (handy for tests). ``now_str`` overrides the
    wall-clock displayed in the header bar (handy for snapshot tests
    that need a deterministic clock).
    """
    img = Image.new("RGB", (CANVAS_W, CANVAS_H), p.BG_PRIMARY)

    # Header band (no outer margin — flush to canvas edges).
    draw_header(
        img,
        0,
        0,
        CANVAS_W,
        hostname=hostname or _read_hostname(),
        state=state,
        now_str=now_str,
    )
    # Layout math:
    #   header           32 px
    #   1 px divider (drawn by header_bar bottom line)
    #   tile row 1      ~124 px
    #   tile gap          8 px
    #   tile row 2      ~124 px
    #   1 px divider (drawn by footer_bar top line)
    #   footer           28 px
    # 32 + 1 + 124 + 8 + 124 + 1 + 28 = 318 px (allowing 2 px slack)
    rows_y = HEADER_HEIGHT + 1
    rows_avail_h = CANVAS_H - HEADER_HEIGHT - 1 - FOOTER_HEIGHT - 1
    tile_h = (rows_avail_h - TILE_GAP) // 2
    tile_w = (CANVAS_W - OUTER_MARGIN * 2 - TILE_GAP) // 2

    col_a_x = OUTER_MARGIN
    col_b_x = OUTER_MARGIN + tile_w + TILE_GAP
    row_a_y = rows_y + 4
    row_b_y = row_a_y + tile_h + TILE_GAP

    # Top row.
    draw_radio_link_tile(img, col_a_x, row_a_y, tile_w, tile_h, state)
    draw_drone_tile(img, col_b_x, row_a_y, tile_w, tile_h, state)
    # Bottom row.
    draw_mesh_tile(img, col_a_x, row_b_y, tile_w, tile_h, state)
    draw_uplink_tile(img, col_b_x, row_b_y, tile_w, tile_h, state)

    # Footer band — drawn last so its top divider lands cleanly above
    # any tile content that might bleed.
    footer_y = CANVAS_H - FOOTER_HEIGHT
    draw_footer(img, 0, footer_y, CANVAS_W, state=state)

    return img


# ──────────────────────────────────────────────────────────────────────
# Snapshot CLI — for dev visual review without a live agent
# ──────────────────────────────────────────────────────────────────────


def _mock_state() -> dict[str, Any]:
    return {
        "link": {
            "rssi_dbm": -67,
            "bitrate_mbps": 20.0,
            "fec_recovered": 1247,
            "fec_lost": 3,
            "channel": 161,
        },
        "drone": {
            "device_id": "drone-AABBCC42F1",
            "fc_mode": "STAB",
            "battery_pct": 87,
            "gps_sats": 11,
            "armed": False,
            "key_fingerprint": "X" * 16,
        },
        "network": {
            "ap_ssid": "ados-groundnode-7591",
            "ap_ip": "10.42.0.1",
            "usb_ip": "10.42.1.1",
            "uplink_type": "eth",
            "uplink_reachable": True,
        },
        "system": {
            "cpu_pct": 22,
            "ram_used_mb": 1234,
            "ram_total_mb": 16384,
            "temp_c": 47,
            "uptime_seconds": 7894,
            "agent_version": "0.12.0",
        },
        "role": {
            "current": "receiver",
            "configured": "receiver",
            "mesh_capable": True,
        },
        "mesh": {
            "up": True,
            "peer_count": 3,
            "selected_gateway": "groundnode-2",
            "partition": False,
            "mesh_id": "12ABCD",
        },
        "cloud": {
            "paired": False,
            "pair_code": "7YTFC7",
            "latency_ms": 12,
        },
        "pairing": {"code": "7YTFC7"},
    }


def _main() -> int:
    parser = argparse.ArgumentParser(
        description="Render the groundnode landscape dashboard to a PNG.",
    )
    parser.add_argument(
        "--snapshot",
        type=Path,
        default=Path("/tmp/groundnode_dashboard.png"),
        help="Output PNG path (default: /tmp/groundnode_dashboard.png)",
    )
    parser.add_argument(
        "--state",
        type=Path,
        default=None,
        help="Path to a JSON file containing the state dict. Defaults to mock state.",
    )
    parser.add_argument(
        "--hostname",
        type=str,
        default=None,
        help="Override hostname rendered in the header bar.",
    )
    parser.add_argument(
        "--mock",
        action="store_true",
        help="(Default if --state not given) Use a synthesized realistic state dict.",
    )
    args = parser.parse_args()

    if args.state is not None:
        state = json.loads(args.state.read_text())
    else:
        state = _mock_state()

    img = render(state, hostname=args.hostname, now_str="13:47:23")
    args.snapshot.parent.mkdir(parents=True, exist_ok=True)
    img.save(args.snapshot, "PNG")
    print(f"snapshot written: {args.snapshot} ({CANVAS_W}x{CANVAS_H})")
    return 0


if __name__ == "__main__":
    raise SystemExit(_main())
