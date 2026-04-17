"""Relay: receiver mDNS timeout, grace period before operator intervenes."""

from __future__ import annotations

import time
from typing import Any, Awaitable, Callable


def render(draw: Any, width: int, height: int, state: dict) -> None:
    mesh = state.get("mesh") or {}
    lost_since_ms = mesh.get("hub_lost_since_ms", 0)
    now_ms = int(time.time() * 1000)
    elapsed_s = max(0, (now_ms - lost_since_ms) // 1000) if lost_since_ms else 0

    draw.text((0, 0), "Mesh: hub lost", fill="white")
    draw.text((0, 18), f"Elapsed: {elapsed_s}s", fill="white")
    draw.text((0, 30), "Waiting for hub...", fill="white")
    draw.text((0, 44), "B1 wait more", fill="white")
    draw.text((0, 52), "B4 go direct", fill="white")


async def _wait(service: Any) -> None:
    service._exit_overlay()


async def _go_direct(service: Any) -> None:
    try:
        await service._http.put(
            f"{service._api_base}/role",
            json={"role": "direct"},
            timeout=5.0,
        )
    except Exception:
        pass
    service._exit_overlay()


BUTTON_ACTIONS: dict[int, Callable[[Any], Awaitable[None]]] = {
    5: _wait,       # B1
    19: _go_direct, # B4
}


def initial_state(service: Any) -> dict:
    return {}
