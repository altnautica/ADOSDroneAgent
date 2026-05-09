#!/usr/bin/env python3
"""Render reference frames for the on-device display.

Captures deterministic snapshots of every screen the SPI LCD renders.
No agent or hardware is touched. State dicts come from in-file
constants modeled on the dashboard module's ``_mock_state()``.

Output tree::

    out/carousel/{link,drone,gcs,net,system}.png
    out/dashboard/dashboard.png
    out/mesh/{unset-boot,role-picker,accept-window,join-scan,
              joined-status,neighbors,hub-unreachable,
              mesh-unavailable,leave-confirm,error-states}.png

Each PNG is 480x320 RGB. Carousel and mesh frames paint the OLED
screen module onto a 128x64 logical canvas, NEAREST-upscale 3x to
384x192, and paste centered on a 480x320 black background. Production
``FrameBufferRenderer`` uses a 4x upscale (512x256) which overflows
the panel horizontally and clips a few logical pixels off both
sides. The 3x ratio used here keeps the full glyph width readable
in static reference frames; the panel still receives the full 4x
upscale at runtime.

Dashboard renders natively at 480x320 via the live tile renderers.

Usage::

    uv run python scripts/dev/render-screen-frames.py
    uv run python scripts/dev/render-screen-frames.py --out /tmp/frames
"""

from __future__ import annotations

import argparse
import sys
import time
from pathlib import Path

from PIL import Image, ImageDraw

HERE = Path(__file__).resolve()
REPO_ROOT = HERE.parents[2]
SRC_ROOT = REPO_ROOT / "src"
if str(SRC_ROOT) not in sys.path:
    sys.path.insert(0, str(SRC_ROOT))

from ados.services.ui.dashboards.groundnode_landscape import (  # noqa: E402
    _mock_state,
    render as render_dashboard,
)
from ados.services.ui.screens import (  # noqa: E402
    drone as screen_drone,
    gcs as screen_gcs,
    link as screen_link,
    net as screen_net,
    system as screen_system,
)
from ados.services.ui.screens.mesh import (  # noqa: E402
    accept_window,
    error_states,
    hub_unreachable,
    join_scan,
    joined_status,
    leave_confirm,
    mesh_unavailable,
    neighbors,
    role_picker,
    unset_boot,
)


LOGICAL_W = 128
LOGICAL_H = 64

LCD_W = 480
LCD_H = 320

UPSCALE = 3


def render_carousel_frame(module, state: dict) -> Image.Image:
    """Render an OLED-mode screen module to a 480x320 RGB PIL image.

    Paint onto a 128x64 logical canvas, NEAREST-upscale by ``UPSCALE``,
    paste centered onto a 480x320 black panel. ``UPSCALE`` of 3 yields
    384x192 (fits with 48 px side margin and 64 px top/bottom band).
    """
    logical = Image.new("RGB", (LOGICAL_W, LOGICAL_H), (0, 0, 0))
    draw = ImageDraw.Draw(logical)
    module.render(draw, LOGICAL_W, LOGICAL_H, state)

    scaled_w = LOGICAL_W * UPSCALE
    scaled_h = LOGICAL_H * UPSCALE
    scaled = logical.resize((scaled_w, scaled_h), resample=Image.NEAREST)

    canvas = Image.new("RGB", (LCD_W, LCD_H), (0, 0, 0))
    x = (LCD_W - scaled_w) // 2
    y = (LCD_H - scaled_h) // 2
    canvas.paste(scaled, (x, y))
    return canvas


def carousel_fixtures() -> list[tuple[str, object, dict]]:
    base = _mock_state()
    base_with_clients = dict(base)
    base_with_clients["gcs"] = {
        "clients": [
            {"id": 1, "type": "kiosk", "ip": "10.42.0.55"},
            {"id": 2, "type": "android", "ip": "10.42.0.71"},
            {"id": 3, "type": "tether", "ip": "10.42.1.10"},
        ],
        "pic_id": 1,
    }
    return [
        ("link", screen_link, base),
        ("drone", screen_drone, base),
        ("gcs", screen_gcs, base_with_clients),
        ("net", screen_net, base),
        ("system", screen_system, base),
    ]


def mesh_fixtures() -> list[tuple[str, object, dict]]:
    now_ms = int(time.time() * 1000)
    out: list[tuple[str, object, dict]] = []

    base_unset = dict(_mock_state())
    base_unset["role"] = {"current": "unset"}
    out.append(("unset-boot", unset_boot, base_unset))

    role_state = dict(_mock_state())
    role_state["role"] = {"current": "direct"}
    role_state["_overlay_state"] = {"role_idx": 1}
    out.append(("role-picker", role_picker, role_state))

    accept_state = dict(_mock_state())
    accept_state["role"] = {"current": "receiver"}
    accept_state["pairing"] = {
        "window": {"closes_at_ms": now_ms + 47_000},
        "pending": [{"device_id": "ados-gs-a4b2"}],
    }
    accept_state["_overlay_state"] = {"cursor": 0}
    out.append(("accept-window", accept_window, accept_state))

    join_state = dict(_mock_state())
    join_state["role"] = {"current": "relay"}
    join_state["mesh"] = {
        "scan": {"found_host": "ados-gs-a4b2.local", "link_quality": -52},
    }
    out.append(("join-scan", join_scan, join_state))

    joined_state = dict(_mock_state())
    joined_state["role"] = {"current": "relay"}
    joined_state["mesh"] = {
        "up": True,
        "peer_count": 3,
        "mesh_id": "12ABCD",
    }
    joined_state["_overlay_state"] = {
        "mesh_id": "12ABCD",
        "receiver_host": "ados-gs-a4b2.local",
    }
    out.append(("joined-status", joined_status, joined_state))

    neighbors_state = dict(_mock_state())
    neighbors_state["role"] = {"current": "relay"}
    neighbors_state["mesh"] = {
        "neighbors": [
            {"mac": "AA:11:22:33", "tq": 245},
            {"mac": "BB:44:55:66", "tq": 232},
            {"mac": "CC:77:88:99", "tq": 198},
        ],
    }
    neighbors_state["_overlay_state"] = {"cursor": 1}
    out.append(("neighbors", neighbors, neighbors_state))

    hub_state = dict(_mock_state())
    hub_state["role"] = {"current": "relay"}
    hub_state["mesh"] = {"hub_lost_since_ms": now_ms - 45_000}
    out.append(("hub-unreachable", hub_unreachable, hub_state))

    out.append(("mesh-unavailable", mesh_unavailable, _mock_state()))

    out.append(("leave-confirm", leave_confirm, _mock_state()))

    error_state = dict(_mock_state())
    error_state["_overlay_state"] = {"code": "E_JOIN_FAILED", "message": ""}
    out.append(("error-states", error_states, error_state))

    return out


def main() -> int:
    parser = argparse.ArgumentParser(description="Render LCD screen reference frames.")
    parser.add_argument(
        "--out",
        type=Path,
        default=HERE.parent / "out",
        help="Output directory (default: scripts/dev/out/)",
    )
    args = parser.parse_args()

    out: Path = args.out
    (out / "carousel").mkdir(parents=True, exist_ok=True)
    (out / "dashboard").mkdir(parents=True, exist_ok=True)
    (out / "mesh").mkdir(parents=True, exist_ok=True)

    written = 0

    for name, module, state in carousel_fixtures():
        img = render_carousel_frame(module, state)
        path = out / "carousel" / f"{name}.png"
        img.save(path, "PNG")
        print(f"  carousel/{name}.png  {img.size[0]}x{img.size[1]}")
        written += 1

    img = render_dashboard(_mock_state(), now_str="13:47:23")
    path = out / "dashboard" / "dashboard.png"
    img.save(path, "PNG")
    print(f"  dashboard/dashboard.png  {img.size[0]}x{img.size[1]}")
    written += 1

    for name, module, state in mesh_fixtures():
        img = render_carousel_frame(module, state)
        path = out / "mesh" / f"{name}.png"
        img.save(path, "PNG")
        print(f"  mesh/{name}.png  {img.size[0]}x{img.size[1]}")
        written += 1

    print(f"done. wrote {written} frames to {out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
