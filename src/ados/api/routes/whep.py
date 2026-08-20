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

from ados.core.logging import get_logger

log = get_logger("api.whep")

router = APIRouter()

# Local MediaMTX media endpoints, fronted so a browser reaches the live video
# plane through the SAME host:port as the REST + WebSocket surface. Advertising
# absolute ``:8889`` / ``:8888`` URLs broke off-LAN (a ``.local`` name a remote
# GCS cannot resolve, or an IP the browser cannot route) and under an HTTPS GCS
# (mixed content); a same-origin ``/whep`` + ``/hls`` path resolves against
# whatever host reached the agent. WHEP (WebRTC) is served per published leg at
# ``:8889/<leg>/whep``, HLS at ``:8888/<leg>/index.m3u8``. PROFILE-AGNOSTIC: the
# on-drone cockpit needs its own proxy, not only the ground station. Module-level
# so tests can swap the upstreams.
_WHEP_BASE = "http://127.0.0.1:8889"
_HLS_BASE = "http://127.0.0.1:8888"

# The primary published leg. A multi-leg node addresses a secondary leg by
# ``?camera=<id>`` (WHEP) or ``/hls/<id>/index.m3u8`` (HLS).
_DEFAULT_CAMERA = "main"


def _camera_id(request: Request) -> str:
    """The requested leg id from ``?camera=``, defaulting to the primary ``main``.

    Validated path-safe (it becomes a mediamtx path segment): alphanumerics plus
    ``-`` / ``_``, matching the roster's own leg-id rule. A bad value is a 400,
    never a path traversal into another mediamtx route.
    """
    camera = request.query_params.get("camera", _DEFAULT_CAMERA)
    if not camera or not all(c.isalnum() or c in "-_" for c in camera):
        raise HTTPException(status_code=400, detail="invalid camera id")
    return camera


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
    upstream_url: str,
    request: Request,
    *,
    camera: str | None = None,
) -> StreamingResponse:
    """Forward a request to a local MediaMTX media endpoint (WHEP or HLS).

    ``upstream_url`` is the absolute loopback target. Reads the request body in
    full (the SDP / SDP-fragment payload is a few KB and MediaMTX expects a known
    Content-Length); the response is streamed back so chunked encodings and HLS
    segments flow through cleanly.

    When ``camera`` is set (a WHEP offer), the upstream ``Location`` — a mediamtx
    resource path like ``/<camera>/whep/<session>`` — is rewritten to this proxy's
    own ``/whep/<session>?camera=<camera>`` so the client tears the session down
    (DELETE) and restarts ICE (PATCH) back through the proxy instead of dialing
    mediamtx's loopback address, which is unreachable from the browser.
    """
    body = await request.body()
    headers = _filter_request_headers(request)
    client = _get_client()

    try:
        upstream = await client.request(
            method,
            upstream_url,
            content=body,
            headers=headers,
        )
    except httpx.ConnectError:
        log.warning("media_upstream_unreachable", url=upstream_url)
        raise HTTPException(
            status_code=503,
            detail="upstream media endpoint unreachable",
        )
    except httpx.TimeoutException:
        log.warning("media_upstream_timeout", url=upstream_url)
        raise HTTPException(
            status_code=504,
            detail="upstream media endpoint timed out",
        )

    response_headers = _filter_response_headers(upstream)
    if camera is not None:
        _rewrite_whep_location(response_headers, camera)
    media_type = upstream.headers.get("content-type")

    log.debug(
        "media_proxy",
        method=method,
        upstream_status=upstream.status_code,
        upstream_url=upstream_url,
    )

    return StreamingResponse(
        content=iter([upstream.content]),
        status_code=upstream.status_code,
        headers=response_headers,
        media_type=media_type,
    )


def _rewrite_whep_location(headers: dict[str, str], camera: str) -> None:
    """Rewrite a mediamtx WHEP ``Location`` (``/<camera>/whep/<session>``) to this
    proxy's ``/whep/<session>?camera=<camera>`` so the session-resource DELETE /
    PATCH route back here. Best-effort: an absent or unexpected Location is left
    untouched (the exchange still succeeds; only explicit teardown is affected).
    """
    prefix = f"/{camera}/whep"
    for key in list(headers):
        if key.lower() != "location":
            continue
        loc = headers[key]
        if loc.startswith(prefix):
            headers[key] = f"/whep{loc[len(prefix):]}?camera={camera}"
        return


@router.post("/whep")
async def whep_offer(request: Request) -> StreamingResponse:
    """Forward a WHEP SDP offer to the requested leg (``?camera=<id>``, default
    the primary ``main``)."""
    camera = _camera_id(request)
    return await _forward(
        "POST", f"{_WHEP_BASE}/{camera}/whep", request, camera=camera
    )


@router.delete("/whep/{session_id}")
async def whep_terminate(session_id: str, request: Request) -> StreamingResponse:
    """Forward a WHEP session termination for the requested leg."""
    camera = _camera_id(request)
    return await _forward(
        "DELETE", f"{_WHEP_BASE}/{camera}/whep/{session_id}", request
    )


@router.patch("/whep/{session_id}")
async def whep_ice_restart(session_id: str, request: Request) -> StreamingResponse:
    """Forward a trickle-ICE SDP fragment (ICE restart) for the requested leg."""
    camera = _camera_id(request)
    return await _forward(
        "PATCH", f"{_WHEP_BASE}/{camera}/whep/{session_id}", request
    )


@router.get("/hls/{path:path}")
async def hls_proxy(path: str, request: Request) -> StreamingResponse:
    """Forward an HLS playlist or segment to the local MediaMTX HLS server,
    preserving the leg subpath (``/hls/<id>/index.m3u8`` → ``:8888/<id>/index.m3u8``)
    so the playlist's relative segment URIs resolve back through this proxy."""
    return await _forward("GET", f"{_HLS_BASE}/{path}", request)
