"""GCS clients screen: list of connected GCS endpoints (max 3)."""

from __future__ import annotations

from typing import Any


def render(draw: Any, width: int, height: int, state: dict) -> None:
    gcs = state.get("gcs") or {}
    clients = gcs.get("clients") or []
    pic = gcs.get("pic_id")

    draw.text((0, 0), "GCS", fill="white")
    draw.text((width - 40, 0), f"n {len(clients)}", fill="white")

    if not clients:
        draw.text((0, 22), "no clients", fill="white")
        return

    y = 14
    for client in clients[:3]:
        label = client.get("type") or client.get("ip") or "--"
        cid = client.get("id")
        badge = "PIC" if cid is not None and cid == pic else ""
        line = f"{label}"[:16]
        if badge:
            line = f"{line} {badge}"
        draw.text((0, y), line, fill="white")
        y += 16
