"""Refuse any request to the residual API that did not arrive through the front.

The native control front (`ados-control`) owns the LAN port and is the single
authenticator for every route it serves or reverse-proxies. This app is the
proxy target. That arrangement rests on one property nothing used to check:
**the only way in is the internal Unix socket**.

Two comments used to assert opposite contracts about this — `server.py` said
the front authenticates everything, `serve.rs` said the residual applies its
own auth — and the gap between them was that neither was enforced. This
middleware makes the true one true.

What it asserts, and why each is the honest form of the check:

* **Transport.** When the app is bound to the internal socket, a request that
  arrived over TCP did not come through the front. uvicorn reports the
  listener in ``scope["server"]``: over ``AF_UNIX`` the port element is
  ``None`` and ``scope["client"]`` is ``None``; over TCP both carry real
  values. That is a property of the listener the request landed on, not a
  header anyone can set.

* **The on-box marker.** The front strips any client-supplied ``X-ADOS-Onbox``
  and re-sets it only when its own loopback check passes, so the value is
  trustworthy *on the internal socket*. It is deliberately NOT required:
  the front forwards legitimate off-box authenticated requests without it,
  and requiring it would refuse every remote operator. What is refused is the
  inverse — the header arriving on a transport the front does not own, which
  can only be a spoof.

When the app is NOT behind the front (``ADOS_API_INTERNAL_SOCKET`` unset, i.e.
a standalone dev run), the transport assertion is skipped: there
is no front, TCP is the intended way in, and refusing it would break the
no-hardware path the repo requires to keep working. The header spoof check
still applies, because nothing legitimate sets it in that posture either.
"""

from __future__ import annotations

import os

from starlette.middleware.base import BaseHTTPMiddleware
from starlette.requests import Request
from starlette.responses import JSONResponse

from ados.api.dual_bind import API_INTERNAL_SOCKET_ENV

#: Set by the native front after its own loopback check, on the forwarded
#: request. Lower-case because ASGI normalises header names.
ONBOX_HEADER = "x-ados-onbox"

_REFUSAL = "This API is reachable only through the agent's control front."


def _arrived_over_unix_socket(request: Request) -> bool:
    """Whether this request landed on an ``AF_UNIX`` listener.

    uvicorn sets ``scope["server"]`` to ``(path, None)`` for a Unix socket and
    ``(host, port)`` for TCP, and leaves ``scope["client"]`` ``None`` for a
    Unix peer because there is no peer address to report. Either alone is
    enough; both are checked so a future server that fills one in does not
    silently turn this into a no-op.
    """
    server = request.scope.get("server")
    unix_listener = isinstance(server, (tuple, list)) and server[1] is None
    return unix_listener and request.scope.get("client") is None


class OnboxOriginMiddleware(BaseHTTPMiddleware):
    """Assert the request came through the control front."""

    def __init__(self, app) -> None:  # noqa: ANN001 - Starlette's own signature
        super().__init__(app)
        # Read once at construction: the binding posture cannot change while
        # the process runs, and re-reading the environment per request would
        # be a per-request syscall on the hot path for a constant.
        self._behind_front = bool(
            os.environ.get(API_INTERNAL_SOCKET_ENV, "").strip()
        )

    async def dispatch(self, request: Request, call_next):  # noqa: ANN001,ANN201
        over_unix = _arrived_over_unix_socket(request)

        if self._behind_front and not over_unix:
            # The front owns the LAN port; this listener should be receiving
            # nothing but its forwarded traffic. A TCP arrival here means
            # something reached the residual API directly, bypassing every
            # gate the front applies.
            return JSONResponse({"detail": _REFUSAL}, status_code=403)

        if not over_unix and ONBOX_HEADER in request.headers:
            # Only the front sets this, and only on the socket it forwards
            # over. Arriving anywhere else it is a client claiming on-box
            # privilege it does not have.
            return JSONResponse({"detail": _REFUSAL}, status_code=403)

        return await call_next(request)
