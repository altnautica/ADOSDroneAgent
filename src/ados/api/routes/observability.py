"""Reverse-proxy bridge from the FastAPI surface to the local logging and
telemetry store's query API.

The store serves its read API on a trusted local Unix socket
(``/run/ados/logd-query.sock``) and on a LAN TCP port. The GCS reaches the
store LAN-direct in the common case. Older or firewalled clients that can only
reach the agent's main HTTP surface on :8080 still need the store's data; this
module mounts a thin forwarder at ``/api/v2/observability/*`` that proxies to
the store over the trusted socket.

Design contract:

* **Pure forwarder.** It copies the path under ``/v1``, the query string, the
  request method, and the response body + status; it adds nothing and stores
  nothing. The store never writes FastAPI state and vice-versa, so the two
  surfaces cannot drift.
* **Auth is inherited, not duplicated.** The proxy lives on the agent's main
  HTTP surface, so the existing ``X-ADOS-Key`` auth middleware already gates it
  before a request reaches this router. The hop from FastAPI to the store runs
  over the trusted local socket and carries no key — being on-box is the trust
  boundary there. No second auth layer is added here.
* **Streaming preserved.** The live tail is Server-Sent-Events and the export is
  a chunked byte stream; both are streamed through without buffering the whole
  body, and a client disconnect propagates to the upstream request.
* **Store-down is a clean 503.** When the socket is absent or refuses the
  connection, the proxy returns the standard error envelope with a 503 so a
  client can cascade to its next tier (the legacy ``/api/logs`` surface or a
  different transport).
"""

from __future__ import annotations

import httpx
from fastapi import APIRouter, Request
from fastapi.responses import JSONResponse, StreamingResponse

from ados.core.logging import get_logger
from ados.core.paths import LOGD_QUERY_SOCK

log = get_logger("api.observability")

router = APIRouter()

# The store's query-API base. The host portion is a placeholder httpx requires;
# the Unix-socket transport routes to the socket regardless of it. Mirrors the
# CLI transport (``cli/logs_transport.py``) so both halves speak to the store
# the same way.
_UPSTREAM_BASE = "http://logd"

# Hop-by-hop headers must not cross the proxy: they are scoped to a single TCP
# hop per RFC 7230 §6.1 and would corrupt the connection if echoed. ``host`` is
# rewritten by httpx for the upstream URL; ``content-length`` and
# ``transfer-encoding`` are recomputed by the responder.
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

# Long enough that a live tail does not idle-timeout (the store sends keep-alive
# comment frames every 15 s) while a query/export still fails fast on a wedged
# store. ``read=None`` removes the read-idle ceiling so a long-lived SSE stream
# is not cut off; the connect leg stays short so a missing store is detected at
# once.
_TIMEOUT = httpx.Timeout(connect=2.0, read=None, write=10.0, pool=2.0)

# Module-level singleton over the trusted Unix socket, created on first use so a
# test suite (or an agent build) that never touches observability pays nothing.
# Tests can override by assigning a custom AsyncClient (e.g. one backed by
# ``httpx.MockTransport``) to ``_client_singleton``.
_client_singleton: httpx.AsyncClient | None = None


def _get_client() -> httpx.AsyncClient:
    """Return the shared upstream client, creating it over the UDS if absent."""
    global _client_singleton
    if _client_singleton is None:
        transport = httpx.AsyncHTTPTransport(uds=str(LOGD_QUERY_SOCK))
        _client_singleton = httpx.AsyncClient(
            base_url=_UPSTREAM_BASE,
            transport=transport,
            timeout=_TIMEOUT,
        )
    return _client_singleton


async def aclose_client() -> None:
    """Close the shared upstream client. Called on app shutdown."""
    global _client_singleton
    if _client_singleton is not None:
        await _client_singleton.aclose()
        _client_singleton = None


def _filter_request_headers(request: Request) -> dict[str, str]:
    """Strip hop-by-hop headers before forwarding to the store. The
    ``X-ADOS-Key`` (if any) is dropped too: the store's socket plane is
    unauthenticated and the agent's own auth already gated this request."""
    out: dict[str, str] = {}
    for key, value in request.headers.items():
        kl = key.lower()
        if kl in _HOP_BY_HOP or kl == "x-ados-key":
            continue
        out[key] = value
    return out


def _filter_response_headers(resp: httpx.Response) -> dict[str, str]:
    """Strip hop-by-hop headers from the upstream response."""
    out: dict[str, str] = {}
    for key, value in resp.headers.items():
        if key.lower() in _HOP_BY_HOP:
            continue
        out[key] = value
    return out


def _unreachable_response() -> JSONResponse:
    """The 503 a client cascades on when the store is not serving."""
    return JSONResponse(
        status_code=503,
        content={
            "error": {
                "code": "service_unavailable",
                "message": "logging store query socket unavailable",
            }
        },
    )


@router.api_route(
    "/v2/observability/{upstream_path:path}",
    methods=["GET"],
    name="observability_proxy",
)
async def observability_proxy(upstream_path: str, request: Request) -> object:
    """Forward a read request to the store's ``/v1`` query API.

    The path tail (everything after ``/api/v2/observability/``) and the query
    string are forwarded verbatim, so ``/api/v2/observability/v1/query?limit=10``
    reaches the store at ``/v1/query?limit=10``. The response is streamed so the
    SSE tail and the chunked export flow through without buffering. Status codes
    and the store's JSON error envelope pass through unchanged; a store that is
    not serving yields a clean 503.
    """
    target = f"/{upstream_path}"
    headers = _filter_request_headers(request)
    client = _get_client()

    upstream_req = client.build_request(
        "GET",
        target,
        params=request.query_params,
        headers=headers,
    )

    try:
        upstream = await client.send(upstream_req, stream=True)
    except (httpx.ConnectError, httpx.ConnectTimeout, FileNotFoundError, OSError):
        # Socket missing or connection refused: the store is not serving.
        log.debug("observability_store_unreachable", path=target)
        return _unreachable_response()
    except httpx.HTTPError as exc:
        log.warning("observability_proxy_error", path=target, error=str(exc))
        return _unreachable_response()

    response_headers = _filter_response_headers(upstream)
    media_type = upstream.headers.get("content-type")

    async def _body_stream():
        try:
            # aiter_bytes streams chunk-by-chunk and decodes any HTTP
            # Content-Encoding. The store never sets one (the jsonl.zst export
            # carries application-level zstd as the body, not as an HTTP
            # encoding), so the bytes are identical to the wire and SSE / chunked
            # streaming flows through without the whole body being buffered.
            async for chunk in upstream.aiter_bytes():
                yield chunk
        finally:
            # Releases the upstream connection on completion AND on a client
            # disconnect (Starlette cancels the generator), so a dropped SSE
            # tail does not leak the upstream request.
            await upstream.aclose()

    return StreamingResponse(
        _body_stream(),
        status_code=upstream.status_code,
        headers=response_headers,
        media_type=media_type,
    )


__all__ = ["router", "aclose_client"]
