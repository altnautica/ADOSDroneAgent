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
  mints a one-shot ticket via ``POST /api/_ws/ticket`` (which the
  native front authenticates with the pairing key) and then
  hands the ticket to ``new WebSocket(url, ["ados-ws-ticket",
  <ticket>])``. The ticket is consumed on first use, is bound to a
  specific scope string, and expires within 30 s. Replaces the
  previous ``?api_key=`` query-string fallback so the pairing key
  never reaches DevTools, HAR exports, or reverse-proxy access logs.

The "scope" string binds a ticket to one logical WebSocket route so a
ticket minted for one stream cannot be replayed against another. The
generic ticket endpoint accepts an arbitrary scope and stamps the
ticket with it; the route's ``authenticate_websocket`` call passes the
scope it expects, and ``consume()`` returns ``True`` only when they
match.

The legacy install-job ticket flow at
``POST /api/plugins/jobs/{job_id}/ticket`` lives on as a back-compat
wrapper around the same store: the scope it uses is ``f"plugin-job:
{job_id}"`` plus a distinct subprotocol marker so existing browser
clients keep working.
"""

from __future__ import annotations

import asyncio
import secrets
import time
from dataclasses import dataclass
from typing import Any

from ados.core.logging import get_logger
from ados.core.ws_ticket import load_pairing_api_key, verify_ticket

log = get_logger("api.ws_auth")


# Generic ticket subprotocol marker used by the unified ticket flow.
# Browser clients send ``["ados-ws-ticket", <ticket-hex>]`` and the
# agent echoes back ``ados-ws-ticket`` in
# ``websocket.accept(subprotocol=...)`` per RFC 6455.
WS_TICKET_PROTOCOL = "ados-ws-ticket"


# Legacy marker for the plugin install-job WebSocket flow. Kept so
# previously shipped GCS bundles keep working through their next
# refresh; both markers consume from the same store.
WS_JOB_TICKET_PROTOCOL = "ados-job-ticket"


@dataclass
class _Ticket:
    scope: str
    issued_at_ms: int
    expires_at_ms: int


class _WsTicketStore:
    """In-memory store of one-shot WebSocket auth tickets.

    Each ticket is a 32-byte (64 hex char) random string bound to a
    single ``scope`` and expires 30 s after issue. ``consume`` is
    one-shot — the second call for the same ticket returns ``False``
    even if the TTL is still active, so a leaked ticket cannot be
    replayed. Pruned opportunistically on every issue to keep the
    dict bounded without a background task.
    """

    DEFAULT_TTL_SECONDS = 30
    HEX_LEN = 64  # 32 bytes of randomness

    def __init__(self) -> None:
        self._tickets: dict[str, _Ticket] = {}
        self._lock = asyncio.Lock()

    async def issue(
        self,
        scope: str,
        *,
        ttl_seconds: int = DEFAULT_TTL_SECONDS,
        now_ms: int | None = None,
    ) -> tuple[str, int]:
        if not scope:
            raise ValueError("ticket scope is required")
        ticket = secrets.token_hex(32)
        issued = int(now_ms if now_ms is not None else time.time() * 1000)
        expires = issued + ttl_seconds * 1000
        async with self._lock:
            self._prune_locked(now_ms=issued)
            self._tickets[ticket] = _Ticket(
                scope=scope,
                issued_at_ms=issued,
                expires_at_ms=expires,
            )
        return ticket, expires

    async def consume(
        self,
        ticket: str,
        *,
        scope: str,
        now_ms: int | None = None,
    ) -> bool:
        if not ticket or len(ticket) != self.HEX_LEN:
            return False
        now = int(now_ms if now_ms is not None else time.time() * 1000)
        async with self._lock:
            entry = self._tickets.pop(ticket, None)
            if entry is None:
                return False
            if entry.expires_at_ms <= now:
                return False
            return entry.scope == scope

    def _prune_locked(self, *, now_ms: int) -> None:
        # Caller holds ``self._lock``. O(n) sweep is fine; the dict
        # never grows beyond a handful of in-flight WS handshakes.
        stale = [t for t, e in self._tickets.items() if e.expires_at_ms <= now_ms]
        for t in stale:
            self._tickets.pop(t, None)

    def _reset_for_tests(self) -> None:
        self._tickets.clear()


# Module-level singleton. The agent has one ticket store across all
# WebSocket routes; ticket-to-scope binding is enforced inside
# ``consume``.
ws_ticket_store = _WsTicketStore()


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
    allow_legacy_job_protocol: bool = False,
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

    ``allow_legacy_job_protocol`` enables the legacy
    ``ados-job-ticket`` marker for callers that previously consumed
    the install-job ticket flow. New routes leave this ``False`` and
    use the unified ``ados-ws-ticket`` marker.
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
    # the same pairing key, so it verifies with no shared store. The
    # legacy ``ados-job-ticket`` marker for the plugin install-job
    # flow still consumes from the in-process store (the plugin
    # runtime mints those itself).
    offered = _extract_subprotocols(websocket)
    if len(offered) >= 2:
        marker, ticket_value = offered[0], offered[1]
        if marker == WS_TICKET_PROTOCOL:
            pairing_key = load_pairing_api_key()
            if pairing_key and verify_ticket(
                ticket_value, expected_scope=scope, api_key=pairing_key
            ):
                return marker
        elif allow_legacy_job_protocol and marker == WS_JOB_TICKET_PROTOCOL:
            if await ws_ticket_store.consume(ticket_value, scope=scope):
                return marker

    await websocket.close(code=4401, reason="auth required")
    return None


__all__ = [
    "WS_TICKET_PROTOCOL",
    "WS_JOB_TICKET_PROTOCOL",
    "authenticate_websocket",
    "ws_ticket_store",
]
