"""Leave-mesh confirm overlay."""

from __future__ import annotations

from typing import Any, Awaitable, Callable


def render(draw: Any, width: int, height: int, state: dict) -> None:
    draw.text((0, 0), "Leave mesh?", fill="white")
    draw.text((0, 18), "Switches role to direct.", fill="white")
    draw.text((0, 30), "Keeps the pair key.", fill="white")
    draw.text((0, 44), "B3 confirm", fill="white")
    draw.text((0, 52), "B4 cancel", fill="white")


async def _confirm(service: Any) -> None:
    try:
        await service._http.put(
            f"{service._api_base}/role",
            json={"role": "direct"},
            timeout=5.0,
        )
    except Exception as exc:
        service._enter_overlay(
            "error_states",
            initial_state={"code": "E_REST_UNAVAILABLE", "message": str(exc)},
        )
        return
    service._exit_overlay()


async def _cancel(service: Any) -> None:
    service._exit_overlay()


BUTTON_ACTIONS: dict[int, Callable[[Any], Awaitable[None]]] = {
    13: _confirm,  # B3
    19: _cancel,   # B4
}


def initial_state(service: Any) -> dict:
    return {}
