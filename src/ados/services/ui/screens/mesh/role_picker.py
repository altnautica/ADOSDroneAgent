"""Role picker overlay: cycle direct/relay/receiver, B3 applies."""

from __future__ import annotations

from typing import Any, Awaitable, Callable

_ROLES = ["direct", "relay", "receiver"]


def render(draw: Any, width: int, height: int, state: dict) -> None:
    overlay = state.get("_overlay_state") or {}
    highlighted = overlay.get("role_idx", 0)
    current = state.get("role", {}).get("current", "direct")

    draw.text((0, 0), "Set role", fill="white")
    draw.text((width - 56, 0), f"now:{current[:8]}", fill="white")

    for i, role in enumerate(_ROLES):
        prefix = ">" if i == highlighted else " "
        draw.text((0, 16 + i * 12), f"{prefix} {role}", fill="white")

    draw.text((0, 52), "B1/B2 cycle  B3 apply", fill="white")


async def _cycle_up(service: Any) -> None:
    overlay = service._overlay_state
    overlay["role_idx"] = (overlay.get("role_idx", 0) - 1) % len(_ROLES)


async def _cycle_down(service: Any) -> None:
    overlay = service._overlay_state
    overlay["role_idx"] = (overlay.get("role_idx", 0) + 1) % len(_ROLES)


async def _apply(service: Any) -> None:
    overlay = service._overlay_state
    target = _ROLES[overlay.get("role_idx", 0)]
    try:
        resp = await service._http.put(
            f"{service._api_base}/role",
            json={"role": target},
            timeout=5.0,
        )
        if resp.status_code == 200:
            # Optimistic update so the role badge reflects the change
            # before the next 1 Hz poll.
            role_block = service._state.setdefault("role", {})
            role_block["current"] = target
            service._exit_overlay()
        else:
            body = resp.json() if resp.content else {}
            code = body.get("error", {}).get("code", "E_UNKNOWN")
            service._enter_overlay(
                "error_states",
                initial_state={"code": code, "message": body.get("error", {}).get("message", "")},
            )
    except Exception as exc:
        service._enter_overlay(
            "error_states",
            initial_state={"code": "E_REST_UNAVAILABLE", "message": str(exc)},
        )


async def _back(service: Any) -> None:
    service._exit_overlay()


# BCM pin ids match OledService.B1..B4.
BUTTON_ACTIONS: dict[int, Callable[[Any], Awaitable[None]]] = {
    5: _cycle_up,    # B1
    6: _cycle_down,  # B2
    13: _apply,      # B3
    19: _back,       # B4
}


def initial_state(service: Any) -> dict:
    current = service._state.get("role", {}).get("current", "direct")
    try:
        idx = _ROLES.index(current)
    except ValueError:
        idx = 0
    return {"role_idx": idx}
