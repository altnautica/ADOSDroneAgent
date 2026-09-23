"""Shared WebSocket authentication helpers.

The native control front authenticates the HTTP surface, but a WebSocket
handshake is upgraded past that HTTP auth layer, so every
``@router.websocket`` route must enforce the paired-key contract itself.

Two accepted credentials per the agent's WebSocket auth contract:

* ``X-ADOS-Key`` header — native clients (the ``ados`` CLI, agent
  integration tests, any non-browser client that controls handshake
  headers).
* ``Sec-WebSocket-Protocol: ados-ws-ticket, <ticket-hex>`` — the
  subprotocol-based ticket flow for browser clients. The GCS first
  mints a short-lived ticket via ``POST /api/_ws/ticket`` (which the
  native front authenticates with the pairing key) and then
  hands the ticket to ``new WebSocket(url, ["ados-ws-ticket",
  <ticket>])``. The ticket is a self-contained HMAC token bound to a
  specific scope string, and expires within 30 s.
  Replaces the previous ``?api_key=`` query-string fallback so the
  pairing key never reaches DevTools, HAR exports, or reverse-proxy
  access logs.

The "scope" string binds a ticket to one logical WebSocket route so a
ticket minted for one stream cannot be replayed against another. The
mint endpoint only issues tickets for the scopes it knows; the route's
``authenticate_websocket`` call passes the scope it expects and the
HMAC verification fails for any other.
"""

from __future__ import annotations

from typing import Any

from ados.core.logging import get_logger
from ados.core.ws_ticket import load_pairing_api_key, verify_ticket

log = get_logger("api.ws_auth")


# Generic ticket subprotocol marker used by the unified ticket flow.
# Browser clients send ``["ados-ws-ticket", <ticket-hex>]`` and the
# agent echoes back ``ados-ws-ticket`` in
# ``websocket.accept(subprotocol=...)`` per RFC 6455.
WS_TICKET_PROTOCOL = "ados-ws-ticket"


def _extract_subprotocols(websocket: Any) -> list[str]:
    """Read the offered WebSocket subprotocols.

    Starlette parses these into ``scope['subprotocols']``; fall back
    to splitting the raw header when ``scope`` is absent (TestClient
    paths in older Starlette versions).
    """
    scope = getattr(websocket, "scope", None) or {}
    offered = scope.get("subprotocols")
    if isinstance(offered, list) and offered:
        return [str(p) for p in offered]
    raw = websocket.headers.get("sec-websocket-protocol")
    if not raw:
        return []
    return [p.strip() for p in raw.split(",") if p.strip()]


async def authenticate_websocket(
    websocket: Any,
    *,
    scope: str,
) -> str | None:
    """Validate either the ``X-ADOS-Key`` header or a one-shot ticket.

    Returns the subprotocol the route should echo back in
    ``websocket.accept(subprotocol=...)`` when the ticket path is
    taken (so the browser handshake completes per RFC 6455), or an
    empty string when the header path is taken (no subprotocol to
    echo), or ``None`` on rejection. The helper closes the socket
    with code ``4401`` before returning ``None`` so the route only
    has to bail out on a falsy result.

    ``scope`` ties the ticket to one logical route. The same string
    must be passed to the ticket-mint endpoint and to this helper.
    """
    # Import lazily to avoid a circular import at module load time
    # (deps -> server -> routes -> ws_auth).
    from ados.api.deps import get_agent_app

    app = get_agent_app()
    pm = getattr(app, "pairing_manager", None)

    # Open posture on an unpaired agent. Matches HTTP middleware so
    # the bench operator can run the wizard before pairing.
    if pm is None or not getattr(pm, "is_paired", False):
        return ""

    configured_key: str | None = None
    try:
        configured_key = app.config.security.api.api_key
    except AttributeError:
        configured_key = None

    api_key = websocket.headers.get("X-ADOS-Key")
    if api_key:
        if configured_key and api_key == configured_key:
            return ""
        if pm.validate_key(api_key):
            return ""
        # Bad header: still try the ticket path before rejecting, in
        # case a buggy intermediary stuck a junk value on the wire.

    # Ticket path. Browsers cannot set custom headers on the
    # WebSocket handshake; the GCS hands the ticket through the
    # subprotocols list instead. Expect at least the marker and one
    # ticket value; ignore any additional entries.
    #
    # The unified ``ados-ws-ticket`` marker carries a self-contained
    # HMAC ticket minted by the native control surface and keyed off
    # the same pairing key, so it verifies with no shared store.
    offered = _extract_subprotocols(websocket)
    if len(offered) >= 2:
        marker, ticket_value = offered[0], offered[1]
        if marker == WS_TICKET_PROTOCOL:
            pairing_key = load_pairing_api_key()
            if pairing_key and verify_ticket(
                ticket_value, expected_scope=scope, api_key=pairing_key
            ):
                return marker

    await websocket.close(code=4401, reason="auth required")
    return None


__all__ = [
    "WS_TICKET_PROTOCOL",
    "authenticate_websocket",
]
