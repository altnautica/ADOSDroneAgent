"""Mesh neighbors list with TQ + last-seen."""

from __future__ import annotations

from typing import Any, Awaitable, Callable


def render(draw: Any, width: int, height: int, state: dict) -> None:
    overlay = state.get("_overlay_state") or {}
    mesh = state.get("mesh") or {}
    neighbors = mesh.get("neighbors") or []
    cursor = overlay.get("cursor", 0)

    draw.text((0, 0), f"Neighbors ({len(neighbors)})", fill="white")

    if not neighbors:
        draw.text((0, 20), "(no peers)", fill="white")
        draw.text((0, 52), "B4 back", fill="white")
        return

    start = max(0, cursor - 1)
    for i, n in enumerate(neighbors[start:start + 3]):
        y = 14 + i * 12
        prefix = ">" if (start + i) == cursor else " "
        mac = (n.get("mac") or "??")[:12]
        tq = n.get("tq", 0)
        draw.text((0, y), f"{prefix} {mac} tq:{tq}", fill="white")

    draw.text((0, 52), "B1/B2 scroll  B4 back", fill="white")


async def _up(service: Any) -> None:
    overlay = service._overlay_state
    neighbors = (service._state.get("mesh") or {}).get("neighbors") or []
    if not neighbors:
        return
    overlay["cursor"] = (overlay.get("cursor", 0) - 1) % len(neighbors)


async def _down(service: Any) -> None:
    overlay = service._overlay_state
    neighbors = (service._state.get("mesh") or {}).get("neighbors") or []
    if not neighbors:
        return
    overlay["cursor"] = (overlay.get("cursor", 0) + 1) % len(neighbors)


async def _back(service: Any) -> None:
    service._exit_overlay()


BUTTON_ACTIONS: dict[int, Callable[[Any], Awaitable[None]]] = {
    5: _up,     # B1
    6: _down,   # B2
    19: _back,  # B4
}


def initial_state(service: Any) -> dict:
    return {"cursor": 0}
