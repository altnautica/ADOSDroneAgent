"""Mesh-unavailable hint screen.

Rendered when the operator opens the Mesh submenu on a node that is not
`mesh_capable` (the `/etc/ados/profile.conf` flag is missing or false).
Tells the operator how to unlock the feature. Any button exits.

This replaces the previous silent behavior where the Mesh submenu
simply vanished on non-mesh-capable nodes, leaving the operator without
a hint that the feature even exists.
"""

from __future__ import annotations

from typing import Any, Awaitable, Callable


def render(draw: Any, width: int, height: int, state: dict) -> None:
    draw.text((0, 0), "Mesh unavailable", fill="white")
    draw.text((0, 14), "Need a 2nd USB WiFi", fill="white")
    draw.text((0, 24), "adapter + mesh deps.", fill="white")
    draw.text((0, 38), "Rerun installer with", fill="white")
    draw.text((0, 48), "--with-mesh on setup.", fill="white")
    draw.text((0, 56), "B4 back", fill="white")


async def _dismiss(service: Any) -> None:
    service._exit_overlay()


# Any button dismisses. The user needs to know this is informational.
BUTTON_ACTIONS: dict[int, Callable[[Any], Awaitable[None]]] = {
    5: _dismiss,   # B1
    6: _dismiss,   # B2
    13: _dismiss,  # B3
    19: _dismiss,  # B4
}


def initial_state(service: Any) -> dict:
    return {}
