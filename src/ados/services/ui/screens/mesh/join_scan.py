"""Relay mDNS scan overlay: show discovered receiver, B1 to join."""

from __future__ import annotations

from typing import Any, Awaitable, Callable


def render(draw: Any, width: int, height: int, state: dict) -> None:
    mesh = state.get("mesh") or {}
    scan = mesh.get("scan") or {}
    host = scan.get("found_host")
    link = scan.get("link_quality")

    draw.text((0, 0), "Join mesh", fill="white")
    draw.text((0, 18), "Scan on bat0...", fill="white")

    if host:
        draw.text((0, 32), f"Found: {host[:18]}", fill="white")
        if link is not None:
            draw.text((0, 44), f"Signal: {link}", fill="white")
        draw.text((0, 52), "B1 join  B4 back", fill="white")
    else:
        draw.text((0, 32), "(searching...)", fill="white")
        draw.text((0, 52), "B4 cancel", fill="white")


async def _send_join(service: Any) -> None:
    mesh = service._state.get("mesh") or {}
    scan = mesh.get("scan") or {}
    host = scan.get("found_host")
    if not host:
        return
    try:
        resp = await service._http.post(
            f"{service._api_base}/pair/join",
            json={"receiver_host": host},
            timeout=60.0,
        )
        if resp.status_code == 200:
            body = resp.json() if resp.content else {}
            service._enter_overlay(
                "joined_status",
                initial_state={
                    "mesh_id": body.get("mesh_id"),
                    "receiver_host": body.get("receiver_host"),
                },
            )
        else:
            body = resp.json() if resp.content else {}
            code = body.get("error", {}).get("code", "E_JOIN_FAILED")
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


BUTTON_ACTIONS: dict[int, Callable[[Any], Awaitable[None]]] = {
    5: _send_join,   # B1
    19: _back,       # B4
}


def initial_state(service: Any) -> dict:
    return {}
