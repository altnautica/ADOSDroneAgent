"""Relay happy-path summary after a successful join."""

from __future__ import annotations

from typing import Any, Awaitable, Callable


def render(draw: Any, width: int, height: int, state: dict) -> None:
    overlay = state.get("_overlay_state") or {}
    mesh = state.get("mesh") or {}
    mesh_id = overlay.get("mesh_id") or mesh.get("mesh_id") or "--"
    receiver = overlay.get("receiver_host") or "--"
    up = mesh.get("up", False)
    peers = mesh.get("peer_count", 0)

    draw.text((0, 0), "Mesh: joined", fill="white")
    draw.text((0, 18), f"Id: {str(mesh_id)[:18]}", fill="white")
    draw.text((0, 30), f"Hub: {str(receiver)[:18]}", fill="white")
    draw.text((0, 42), f"Up: {'yes' if up else 'no'}  Peers: {peers}", fill="white")
    draw.text((0, 52), "B4 back to status", fill="white")


async def _back(service: Any) -> None:
    service._exit_overlay()


BUTTON_ACTIONS: dict[int, Callable[[Any], Awaitable[None]]] = {
    19: _back,  # B4
}


def initial_state(service: Any) -> dict:
    return {}
