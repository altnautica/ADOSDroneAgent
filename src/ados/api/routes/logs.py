"""Log viewing routes, sourced from the local logging and telemetry store.

These two endpoints keep the legacy response shape that older Mission Control
clients expect, but they no longer hold their own copy of the logs. The agent's
log records flow to the durable on-disk store over the structlog socket handler
(``core/logd_ship.py``); these endpoints read them back from the store's query
API over its trusted local Unix socket.

* ``GET /api/logs`` — recent entries, mapped from the store's ``/v1/query`` rows
  to the legacy ``{ timestamp, level, logger, message }`` tuple. If the store is
  not reachable the endpoint degrades to an empty list with a warning rather
  than a 500: losing history degrades debugging, not flight.
* ``GET /api/logs/stream`` — a Server-Sent-Events stream proxied from the
  store's ``/v1/tail`` and re-mapped to the legacy SSE frame shape.

The store survives reboots and is reachable when the network is down, which is
exactly when an in-memory buffer was least useful. The stderr/journald sink
remains the always-on primary; the store is the durable secondary that these
endpoints read.
"""

from __future__ import annotations

import json
from datetime import datetime, timezone
from typing import Any

import httpx
from fastapi import APIRouter, Query
from fastapi.responses import JSONResponse, StreamingResponse

from ados.core.logging import get_logger
from ados.core.paths import LOGD_QUERY_SOCK

log = get_logger("api.logs")

router = APIRouter()

# The store's query-API base over the trusted Unix socket. The host portion is a
# placeholder httpx requires; the UDS transport routes to the socket regardless.
_UPSTREAM_BASE = "http://logd"

# Hard ceiling on the page size the legacy surface will request from the store.
# The legacy clients ask for tens of entries; this caps a pathological request.
_MAX_LIMIT = 1000

# Connect fast so a missing store degrades the endpoint at once; the read leg is
# bounded because the legacy query is a single bounded page.
_QUERY_TIMEOUT = httpx.Timeout(connect=2.0, read=10.0, write=5.0, pool=2.0)

# The live tail has no read-idle ceiling (the store sends keep-alive comment
# frames); the connect leg still fails fast on a missing store.
_TAIL_TIMEOUT = httpx.Timeout(connect=2.0, read=None, write=5.0, pool=2.0)

# Module-level singletons over the UDS, created on first use. Separate clients so
# the unbounded-read tail timeout never leaks onto the bounded query. Tests can
# override either by assignment (e.g. an ``httpx.MockTransport``-backed client).
_query_client: httpx.AsyncClient | None = None
_tail_client: httpx.AsyncClient | None = None


def _get_query_client() -> httpx.AsyncClient:
    global _query_client
    if _query_client is None:
        transport = httpx.AsyncHTTPTransport(uds=str(LOGD_QUERY_SOCK))
        _query_client = httpx.AsyncClient(
            base_url=_UPSTREAM_BASE, transport=transport, timeout=_QUERY_TIMEOUT
        )
    return _query_client


def _get_tail_client() -> httpx.AsyncClient:
    global _tail_client
    if _tail_client is None:
        transport = httpx.AsyncHTTPTransport(uds=str(LOGD_QUERY_SOCK))
        _tail_client = httpx.AsyncClient(
            base_url=_UPSTREAM_BASE, transport=transport, timeout=_TAIL_TIMEOUT
        )
    return _tail_client


async def aclose_clients() -> None:
    """Close the shared upstream clients. Called on app shutdown."""
    global _query_client, _tail_client
    if _query_client is not None:
        await _query_client.aclose()
        _query_client = None
    if _tail_client is not None:
        await _tail_client.aclose()
        _tail_client = None


def _legacy_entry(row: dict[str, Any]) -> dict[str, Any]:
    """Map one store log row onto the legacy ``/api/logs`` entry shape.

    The store row is ``{ id, ts_us, session, source, level, target, msg,
    fields }``; the legacy consumer expects ``{ seq, timestamp, level, logger,
    message }`` with an ISO-8601 timestamp and an upper-case level name.
    """
    ts_us = row.get("ts_us")
    timestamp: str
    if isinstance(ts_us, (int, float)):
        timestamp = datetime.fromtimestamp(
            ts_us / 1_000_000, tz=timezone.utc
        ).isoformat()
    else:
        timestamp = datetime.now(tz=timezone.utc).isoformat()
    return {
        "seq": row.get("id"),
        "timestamp": timestamp,
        "level": str(row.get("level", "")).upper(),
        "logger": row.get("target") or row.get("source") or "",
        "message": row.get("msg", ""),
    }


@router.get("/logs")
async def get_logs(
    level: str | None = Query(None),
    service: str | None = Query(None),
    limit: int = Query(50, ge=1, le=_MAX_LIMIT),
    offset: int = Query(0, ge=0),
):
    """Recent log entries, sourced from the durable store.

    Maps the store's rows to the legacy response shape so existing clients keep
    working. If the store is unreachable the endpoint returns an empty list with
    a ``warning`` field instead of a 500 — history is observability, not flight.
    """
    # Ask the store for enough rows to satisfy the offset window, then page in
    # Python so the legacy offset/limit contract is honored without leaking the
    # store's keyset-cursor model to the legacy caller.
    want = min(offset + limit, _MAX_LIMIT)
    params: dict[str, Any] = {"kind": "logs", "limit": want}
    if level:
        params["level"] = level.lower()
    if service:
        params["source"] = service

    client = _get_query_client()
    try:
        resp = await client.get("/v1/query", params=params)
    except (httpx.ConnectError, httpx.ConnectTimeout, FileNotFoundError, OSError):
        log.warning("logs_store_unreachable")
        return JSONResponse(
            content={
                "entries": [],
                "total": 0,
                "limit": limit,
                "offset": offset,
                "warning": "logging store unavailable",
            }
        )
    except httpx.HTTPError as exc:
        log.warning("logs_store_error", error=str(exc))
        return JSONResponse(
            content={
                "entries": [],
                "total": 0,
                "limit": limit,
                "offset": offset,
                "warning": "logging store query failed",
            }
        )

    if resp.status_code >= 400:
        log.warning("logs_store_status", status=resp.status_code)
        return JSONResponse(
            content={
                "entries": [],
                "total": 0,
                "limit": limit,
                "offset": offset,
                "warning": f"logging store returned {resp.status_code}",
            }
        )

    try:
        body = resp.json()
    except ValueError:
        return JSONResponse(
            content={
                "entries": [],
                "total": 0,
                "limit": limit,
                "offset": offset,
                "warning": "logging store response was not JSON",
            }
        )

    rows = body.get("data") if isinstance(body, dict) else None
    if not isinstance(rows, list):
        rows = []

    # The store returns newest-first; the service filter is already applied
    # store-side via ``source``, but it matches a source prefix loosely there,
    # so re-apply the legacy substring semantics here for parity.
    mapped = [_legacy_entry(r) for r in rows if isinstance(r, dict)]
    if service:
        mapped = [e for e in mapped if service in e.get("logger", "")]
    if level:
        level_upper = level.upper()
        mapped = [e for e in mapped if e["level"] == level_upper]

    total = len(mapped)
    window = mapped[offset : offset + limit]
    return {
        "entries": window,
        "total": total,
        "limit": limit,
        "offset": offset,
    }


@router.get("/logs/stream")
async def stream_logs(
    level: str | None = Query(None),
    service: str | None = Query(None),
):
    """Server-Sent Events stream proxied from the store's live tail.

    Each store tail row is re-mapped to the legacy ``data: <json>`` frame so
    existing EventSource clients keep working. A replay of the most recent
    entries is requested so a fresh stream shows recent context, matching the
    old snapshot behavior. Keep-alive comment frames pass through. If the store
    is unreachable the stream closes cleanly so the client reconnects.
    """
    params: dict[str, Any] = {"kind": "logs", "replay": 100}
    if level:
        params["level"] = level.lower()
    if service:
        params["source"] = service

    client = _get_tail_client()

    async def gen():
        upstream_req = client.build_request("GET", "/v1/tail", params=params)
        try:
            upstream = await client.send(upstream_req, stream=True)
        except (httpx.ConnectError, httpx.ConnectTimeout, FileNotFoundError, OSError):
            log.warning("logs_stream_store_unreachable")
            yield ": logging store unavailable\n\n"
            return
        except httpx.HTTPError as exc:
            log.warning("logs_stream_store_error", error=str(exc))
            yield ": logging store stream failed\n\n"
            return

        try:
            if upstream.status_code >= 400:
                yield ": logging store stream error\n\n"
                return
            async for line in upstream.aiter_lines():
                if not line:
                    continue
                if line.startswith(":"):
                    # Pass keep-alive / notice comment frames straight through.
                    yield f"{line}\n\n"
                    continue
                if not line.startswith("data:"):
                    continue
                payload = line[len("data:") :].strip()
                if not payload:
                    continue
                try:
                    row = json.loads(payload)
                except json.JSONDecodeError:
                    continue
                # The store may publish a "lagged" notice frame on the tail;
                # forward it as a comment so the legacy client does not try to
                # render it as a log entry.
                if isinstance(row, dict) and row.get("kind") == "lagged":
                    yield ": tail lagged\n\n"
                    continue
                if not isinstance(row, dict):
                    continue
                yield f"data: {json.dumps(_legacy_entry(row))}\n\n"
        finally:
            # Releases the upstream connection on completion AND on a client
            # disconnect (Starlette cancels the generator).
            await upstream.aclose()

    return StreamingResponse(gen(), media_type="text/event-stream")


__all__ = ["router", "aclose_clients"]
