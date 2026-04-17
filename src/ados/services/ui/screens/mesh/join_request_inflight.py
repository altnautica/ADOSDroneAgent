"""Relay join-inflight overlay. POST /pair/join is blocking, so this
screen is reached transiently only if the join is driven asynchronously
from a future release. Kept as a placeholder renderer so the overlay
registry has the screen available."""

from __future__ import annotations

import time
from typing import Any, Awaitable, Callable


def render(draw: Any, width: int, height: int, state: dict) -> None:
    overlay = state.get("_overlay_state") or {}
    started = overlay.get("started_ms", 0)
    now_ms = int(time.time() * 1000)
    elapsed_s = max(0, (now_ms - started) // 1000) if started else 0

    draw.text((0, 0), "Join mesh", fill="white")
    draw.text((0, 20), "Requesting...", fill="white")
    draw.text((0, 36), f"Elapsed: {elapsed_s}s", fill="white")
    draw.text((0, 52), "B4 cancel", fill="white")


async def _cancel(service: Any) -> None:
    service._exit_overlay()


BUTTON_ACTIONS: dict[int, Callable[[Any], Awaitable[None]]] = {
    19: _cancel,  # B4
}


def initial_state(service: Any) -> dict:
    return {"started_ms": int(time.time() * 1000)}
