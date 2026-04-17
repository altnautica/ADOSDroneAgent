"""Receiver Accept-window overlay: 60 s countdown + pending relay list."""

from __future__ import annotations

import time
from typing import Any, Awaitable, Callable


def render(draw: Any, width: int, height: int, state: dict) -> None:
    pair = state.get("pairing") or {}
    window = pair.get("window") or {}
    pending = pair.get("pending") or []
    closes_at_ms = window.get("closes_at_ms", 0)
    overlay = state.get("_overlay_state") or {}
    cursor = overlay.get("cursor", 0)

    now_ms = int(time.time() * 1000)
    remaining_s = max(0, (closes_at_ms - now_ms) // 1000) if closes_at_ms else 0

    draw.text((0, 0), "Accept relay", fill="white")
    draw.text((width - 40, 0), f"{remaining_s:>3}s", fill="white")

    if not pending:
        draw.text((0, 18), "Waiting for a relay...", fill="white")
    else:
        start = max(0, cursor - 1)
        for i, req in enumerate(pending[start:start + 3]):
            y = 18 + i * 12
            prefix = ">" if (start + i) == cursor else " "
            device_id = (req.get("device_id") or "??")[:14]
            draw.text((0, y), f"{prefix} {device_id}", fill="white")

    draw.text((0, 52), "B1 approve  B4 close", fill="white")


async def _cursor_up(service: Any) -> None:
    overlay = service._overlay_state
    pending = (service._state.get("pairing") or {}).get("pending") or []
    if not pending:
        return
    overlay["cursor"] = (overlay.get("cursor", 0) - 1) % len(pending)


async def _cursor_down(service: Any) -> None:
    overlay = service._overlay_state
    pending = (service._state.get("pairing") or {}).get("pending") or []
    if not pending:
        return
    overlay["cursor"] = (overlay.get("cursor", 0) + 1) % len(pending)


async def _approve(service: Any) -> None:
    pending = (service._state.get("pairing") or {}).get("pending") or []
    if not pending:
        return
    cursor = service._overlay_state.get("cursor", 0)
    if cursor >= len(pending):
        return
    device_id = pending[cursor].get("device_id")
    if not device_id:
        return
    try:
        resp = await service._http.post(
            f"{service._api_base}/pair/approve/{device_id}",
            timeout=10.0,
        )
        if resp.status_code == 200:
            # Next /pair/pending poll will drop the approved device.
            service._overlay_state["cursor"] = 0
        elif resp.status_code == 410:
            service._enter_overlay(
                "error_states",
                initial_state={"code": "E_PAIR_WINDOW_EXPIRED", "message": ""},
            )
        else:
            body = resp.json() if resp.content else {}
            code = body.get("error", {}).get("code", "E_APPROVE_FAILED")
            service._enter_overlay(
                "error_states",
                initial_state={"code": code, "message": body.get("error", {}).get("message", "")},
            )
    except Exception as exc:
        service._enter_overlay(
            "error_states",
            initial_state={"code": "E_REST_UNAVAILABLE", "message": str(exc)},
        )


async def _close_window(service: Any) -> None:
    try:
        await service._http.post(
            f"{service._api_base}/pair/close",
            timeout=3.0,
        )
    except Exception:
        # Best-effort: the window expires on its own anyway.
        pass
    service._exit_overlay()


BUTTON_ACTIONS: dict[int, Callable[[Any], Awaitable[None]]] = {
    5: _approve,       # B1
    6: _cursor_up,     # B2
    13: _cursor_down,  # B3
    19: _close_window, # B4
}


async def on_enter(service: Any) -> None:
    """Open the pairing window when the operator enters the overlay."""
    try:
        await service._http.post(
            f"{service._api_base}/pair/accept",
            json={"duration_s": 60},
            timeout=3.0,
        )
    except Exception as exc:
        service._enter_overlay(
            "error_states",
            initial_state={"code": "E_REST_UNAVAILABLE", "message": str(exc)},
        )
    # Start high-rate polling of /pair/pending while the overlay is live.
    service._start_pairing_poll()


async def on_exit(service: Any) -> None:
    service._stop_pairing_poll()


def initial_state(service: Any) -> dict:
    return {"cursor": 0}
