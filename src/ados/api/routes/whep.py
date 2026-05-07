"""WHEP reverse-proxy mounted at the root path.

The local MediaMTX instance serves WHEP at ``http://127.0.0.1:8889/main/whep``.
Native Android WebRTC clients on the wireless AP expect the offer/answer
exchange to live at ``http://<agent-host>:8080/whep`` so they can reach
the agent's REST + WebSocket surface and the video plane through a
single host:port. Reconfiguring MediaMTX listen ports would conflict
with its captive defaults across upstream version bumps; a thin proxy
that streams the request and response bodies through is cheaper and
upgrade-safe.

Routes (all gated to the ground-station profile, all forwarded to the
local MediaMTX instance):

* ``POST   /whep``                  — initial SDP offer/answer exchange
* ``DELETE /whep/{session_id}``     — terminate the WHEP session
* ``PATCH  /whep/{session_id}``     — ICE restart (trickle SDP fragment)

The upstream returns a relative ``Location`` header (e.g.
``/main/whep/<sessionid>``) on the POST response. The Android client
dereferences it through the same proxy because both the POST and the
session-resource paths land here, so the header is forwarded
unmodified. MediaMTX session resources persist across PATCH calls; a
shared ``httpx.AsyncClient`` keeps the connection pool alive across
the lifetime of the FastAPI app so per-request TCP setup costs do not
land on the SDP latency budget.
"""

from __future__ import annotations

import httpx
from fastapi import APIRouter, HTTPException, Request
from fastapi.responses import StreamingResponse

from ados.api.routes.ground_station._common import _require_ground_profile
from ados.core.logging import get_logger

log = get_logger("api.whep")

router = APIRouter()

# Local MediaMTX WHEP target. The path component is the published
# stream name; the ground-station mediamtx config publishes ``main`` so
# the resource path is ``/main/whep``. Module-level so tests can swap.
_UPSTREAM_BASE = "http://127.0.0.1:8889"
_UPSTREAM_PATH = "/main/whep"

# Headers we must NOT forward verbatim on either leg of the proxy.
# ``Host`` is rewritten by httpx based on the upstream URL. The hop-by-
# hop headers below are scoped to a single TCP hop per RFC 7230 §6.1
# and would corrupt the connection if echoed across the proxy.
_HOP_BY_HOP = frozenset(
    {
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "host",
        "content-length",
    }
)


# Module-level singleton. Initialised on first use so test suites that
# never touch the WHEP routes do not pay the connection-pool startup
# cost. Tests can override by assigning a custom AsyncClient (e.g.
# backed by httpx.MockTransport) to ``_client_singleton``.
_client_singleton: httpx.AsyncClient | None = None


def _get_client() -> httpx.AsyncClient:
    """Return the shared upstream client, creating it if absent."""
    global _client_singleton
    if _client_singleton is None:
        # Generous timeouts on the connect leg, tighter on the read leg
        # because the SDP exchange is small and a stalled MediaMTX
        # should fail fast rather than hold the phone's renegotiation.
        _client_singleton = httpx.AsyncClient(
            timeout=httpx.Timeout(connect=2.0, read=10.0, write=5.0, pool=2.0),
            follow_redirects=False,
        )
    return _client_singleton


def _filter_request_headers(req: Request) -> dict[str, str]:
    """Strip hop-by-hop headers before forwarding to upstream."""
    out: dict[str, str] = {}
    for key, value in req.headers.items():
        if key.lower() in _HOP_BY_HOP:
            continue
        out[key] = value
    return out


def _filter_response_headers(resp: httpx.Response) -> dict[str, str]:
    """Strip hop-by-hop headers before returning the upstream response."""
    out: dict[str, str] = {}
    for key, value in resp.headers.items():
        if key.lower() in _HOP_BY_HOP:
            continue
        out[key] = value
    return out


async def _forward(
    method: str,
    upstream_path: str,
    request: Request,
) -> StreamingResponse:
    """Forward a request to the local MediaMTX WHEP endpoint.

    Reads the request body in full because the SDP / SDP-fragment
    payload is small (a few KB at most) and MediaMTX expects a known
    Content-Length. The response is streamed back to the caller so
    chunked transfer encodings from MediaMTX flow through cleanly.
    """
    _require_ground_profile()

    body = await request.body()
    headers = _filter_request_headers(request)
    upstream_url = f"{_UPSTREAM_BASE}{upstream_path}"
    client = _get_client()

    try:
        upstream = await client.request(
            method,
            upstream_url,
            content=body,
            headers=headers,
            params=request.query_params,
        )
    except httpx.ConnectError:
        log.warning("whep_upstream_unreachable", url=upstream_url)
        raise HTTPException(
            status_code=503,
            detail="upstream WHEP endpoint unreachable",
        )
    except httpx.TimeoutException:
        log.warning("whep_upstream_timeout", url=upstream_url)
        raise HTTPException(
            status_code=504,
            detail="upstream WHEP endpoint timed out",
        )

    response_headers = _filter_response_headers(upstream)
    media_type = upstream.headers.get("content-type")

    log.debug(
        "whep_proxy",
        method=method,
        upstream_status=upstream.status_code,
        upstream_path=upstream_path,
    )

    return StreamingResponse(
        content=iter([upstream.content]),
        status_code=upstream.status_code,
        headers=response_headers,
        media_type=media_type,
    )


@router.post("/whep")
async def whep_offer(request: Request) -> StreamingResponse:
    """Forward a WHEP SDP offer to the local MediaMTX instance."""
    return await _forward("POST", _UPSTREAM_PATH, request)


@router.delete("/whep/{session_id}")
async def whep_terminate(session_id: str, request: Request) -> StreamingResponse:
    """Forward a WHEP session termination request."""
    return await _forward("DELETE", f"{_UPSTREAM_PATH}/{session_id}", request)


@router.patch("/whep/{session_id}")
async def whep_ice_restart(session_id: str, request: Request) -> StreamingResponse:
    """Forward a trickle-ICE SDP fragment for ICE restart."""
    return await _forward("PATCH", f"{_UPSTREAM_PATH}/{session_id}", request)
