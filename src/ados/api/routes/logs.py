"""Log viewing routes."""

from __future__ import annotations

import asyncio
import json
import logging
from collections import deque

from fastapi import APIRouter, Query
from fastapi.responses import StreamingResponse

router = APIRouter()

# In-memory log buffer. Capacity caps memory usage; on busy boards the
# buffer wraps every few minutes so the dashboard surfaces the most
# recent activity and a "since restart" hint exposes the wrap.
_LOG_BUFFER_CAP = 5000
_log_buffer: deque[dict] = deque(maxlen=_LOG_BUFFER_CAP)
_log_event = asyncio.Event()


class BufferHandler(logging.Handler):
    """Captures log records into an in-memory ring buffer."""

    def emit(self, record: logging.LogRecord) -> None:
        _log_buffer.append({
            "timestamp": self.format(record) if not record.created else record.created,
            "level": record.levelname,
            "logger": record.name,
            "message": record.getMessage(),
        })
        # Wake up any SSE subscribers. Best-effort: the handler runs
        # under the logging lock and ``set()`` is safe from any thread
        # because the underlying event loop is asyncio's default.
        try:
            _log_event.set()
        except Exception:
            pass


def install_log_buffer() -> None:
    """Install the buffer handler on the root logger."""
    handler = BufferHandler()
    handler.setLevel(logging.DEBUG)
    logging.getLogger().addHandler(handler)


@router.get("/logs")
async def get_logs(
    level: str | None = Query(None),
    service: str | None = Query(None),
    limit: int = Query(50, ge=1, le=_LOG_BUFFER_CAP),
    offset: int = Query(0, ge=0),
):
    """Recent log entries with optional filtering."""
    entries = list(_log_buffer)
    buffer_size = len(entries)

    if level:
        level_upper = level.upper()
        entries = [e for e in entries if e["level"] == level_upper]

    if service:
        entries = [e for e in entries if service in e.get("logger", "")]

    total = len(entries)
    entries = entries[offset: offset + limit]

    return {
        "entries": entries,
        "total": total,
        "limit": limit,
        "offset": offset,
        "buffer_size": buffer_size,
        "buffer_cap": _LOG_BUFFER_CAP,
    }


@router.get("/logs/stream")
async def stream_logs(
    level: str | None = Query(None),
    service: str | None = Query(None),
):
    """Server-Sent Events stream of new log entries.

    Each entry produces one ``data: <json>\\n\\n`` frame. The stream
    sends an initial snapshot of the last 100 buffered entries, then
    appends each new entry as it arrives. Clients reconnect with the
    EventSource API; backpressure is best-effort (slow consumers miss
    entries silently rather than blocking the logger).
    """

    async def gen():
        # Initial snapshot of the most-recent 100 entries.
        snapshot = list(_log_buffer)[-100:]
        for e in snapshot:
            if level and e["level"] != level.upper():
                continue
            if service and service not in e.get("logger", ""):
                continue
            yield f"data: {json.dumps(e)}\n\n"

        last_seen = id(snapshot[-1]) if snapshot else 0
        try:
            while True:
                try:
                    await asyncio.wait_for(_log_event.wait(), timeout=15.0)
                except asyncio.TimeoutError:
                    # No new entries within the window — flush a
                    # keep-alive comment to keep proxies from idle-closing
                    # the connection, then loop back into wait.
                    yield ": keep-alive\n\n"
                    continue
                _log_event.clear()
                tail = list(_log_buffer)
                # Emit only entries after the last one we forwarded.
                emit = False
                for e in tail:
                    if not emit:
                        if id(e) == last_seen:
                            emit = True
                        continue
                    if level and e["level"] != level.upper():
                        continue
                    if service and service not in e.get("logger", ""):
                        continue
                    yield f"data: {json.dumps(e)}\n\n"
                if tail:
                    last_seen = id(tail[-1])
        except asyncio.CancelledError:
            # Client disconnected. Let the generator unwind cleanly.
            return

    return StreamingResponse(gen(), media_type="text/event-stream")
