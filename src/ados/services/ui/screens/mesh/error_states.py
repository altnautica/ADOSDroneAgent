"""Unified error presentation for the mesh flows.

Switches on `state['_overlay_state']['code']`. The caller sets the code
when entering the overlay; defaults to a generic message when the code
is unknown. B4 clears the overlay.
"""

from __future__ import annotations

from typing import Any, Awaitable, Callable

_MESSAGES: dict[str, tuple[str, str]] = {
    "E_NOT_PAIRED": ("Not paired", "Pair with a receiver first"),
    "E_PAIR_WINDOW_EXPIRED": ("Window expired", "Receiver closed accept"),
    "E_PAIR_REQUEST_NOT_FOUND": ("Not pending", "Request dropped or expired"),
    "E_WRONG_ROLE": ("Wrong role", "Action needs a different role"),
    "E_MESH_NOT_CAPABLE": ("No mesh HW", "Second WiFi dongle missing"),
    "E_MESH_NOT_INITIALIZED": ("No mesh id", "Run pair or factory reset"),
    "E_INVALID_ROLE": ("Invalid role", "Role must be direct/relay/receiver"),
    "E_JOIN_TIMEOUT": ("Join timed out", "No invite from receiver"),
    "E_JOIN_FAILED": ("Join failed", "Check receiver accept window"),
    "E_REST_UNAVAILABLE": ("Agent offline", "REST call did not respond"),
    "E_MESH_PARTITION": ("Mesh split", "Some peers unreachable"),
    "E_PSK_MISMATCH": ("PSK mismatch", "Different deployment"),
    "E_BATCTL_UNAVAILABLE": ("batctl missing", "Install with --with-mesh"),
}


def render(draw: Any, width: int, height: int, state: dict) -> None:
    overlay = state.get("_overlay_state") or {}
    code = overlay.get("code", "E_UNKNOWN")
    extra = overlay.get("message") or ""
    title, body = _MESSAGES.get(code, ("Error", code))

    draw.text((0, 0), "! " + title, fill="white")
    draw.text((0, 20), body, fill="white")
    if extra:
        draw.text((0, 34), str(extra)[:21], fill="white")
    draw.text((0, 52), "B4 dismiss", fill="white")


async def _clear(service: Any) -> None:
    service._exit_overlay()


BUTTON_ACTIONS: dict[int, Callable[[Any], Awaitable[None]]] = {
    19: _clear,  # B4
}


def initial_state(service: Any) -> dict:
    return {}
