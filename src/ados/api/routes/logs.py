"""Log viewing routes."""

from __future__ import annotations

import asyncio
import json
import logging
from collections import deque
from itertools import count

from fastapi import APIRouter, Query
from fastapi.responses import StreamingResponse

router = APIRouter()

# In-memory log buffer. Capacity caps memory usage; on busy boards the
# buffer wraps every few minutes so the dashboard surfaces the most
# recent activity and a "since restart" hint exposes the wrap.
_LOG_BUFFER_CAP = 5000
_log_buffer: deque[dict] = deque(maxlen=_LOG_BUFFER_CAP)
_log_event = asyncio.Event()
_log_seq = count(1)
_main_loop: asyncio.AbstractEventLoop | None = None


def bind_loop(loop: asyncio.AbstractEventLoop) -> None:
    """Capture the main event loop so logging-thread wakeups are safe.

    ``logging.Handler.emit`` runs under the logging lock and can be
    called from any thread. ``asyncio.Event.set()`` is not thread-safe,
    so the handler routes the wake-up through
    ``loop.call_soon_threadsafe`` against the loop captured here.
    """
    global _main_loop
    _main_loop = loop


def _wake_subscribers() -> None:
    loop = _main_loop
    if loop is None:
        # No loop bound yet (test harness, early boot). The next subscriber
        # tick picks up via the 15 s keep-alive fallback.
        return
    try:
        loop.call_soon_threadsafe(_log_event.set)
    except RuntimeError:
        # Loop closed during shutdown — best-effort silent drop.
        pass


class BufferHandler(logging.Handler):
    """Captures log records into an in-memory ring buffer."""

    def emit(self, record: logging.LogRecord) -> None:
        _log_buffer.append({
            "seq": next(_log_seq),
            "timestamp": self.format(record) if not record.created else record.created,
            "level": record.levelname,
            "logger": record.name,
            "message": record.getMessage(),
        })
        # Wake any SSE subscribers from whichever thread emitted this
        # record. Routed through call_soon_threadsafe because the
        # handler is reachable from non-asyncio worker threads.
        _wake_subscribers()


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
    # Bind the loop lazily on the first stream open so worker-thread
    # log writes can wake subscribers thread-safely.
    if _main_loop is None:
        bind_loop(asyncio.get_running_loop())

    async def gen():
        # Initial snapshot of the most-recent 100 entries. Track the
        # last-seen sequence number so we resume cleanly across buffer
        # wraps; id()-based dedup was unsafe because CPython recycles
        # ids of freed deque entries.
        snapshot = list(_log_buffer)[-100:]
        for e in snapshot:
            if level and e["level"] != level.upper():
                continue
            if service and service not in e.get("logger", ""):
                continue
            yield f"data: {json.dumps(e)}\n\n"

        last_seen_seq = snapshot[-1]["seq"] if snapshot else 0
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
                # Emit every entry whose monotonic sequence number is
                # past the last one we forwarded. Even when the deque
                # wraps and last_seen_seq is no longer in the buffer
                # this still picks up correctly because we compare
                # numerically rather than by reference.
                for e in tail:
                    if e["seq"] <= last_seen_seq:
                        continue
                    if level and e["level"] != level.upper():
                        continue
                    if service and service not in e.get("logger", ""):
                        continue
                    yield f"data: {json.dumps(e)}\n\n"
                if tail:
                    last_seen_seq = tail[-1]["seq"]
        except asyncio.CancelledError:
            # Client disconnected. Let the generator unwind cleanly.
            return

    return StreamingResponse(gen(), media_type="text/event-stream")
