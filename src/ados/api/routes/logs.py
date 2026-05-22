"""Log viewing routes."""

from __future__ import annotations

import asyncio
import json
import logging
import os
from collections import deque
from itertools import count

from fastapi import APIRouter, HTTPException, Query
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

# Per-subscriber fanout. Replaces the previous "every SSE client re-reads
# the entire deque on every wake" pattern with a bounded
# ``asyncio.Queue`` per connection. A single fanout task drains
# ``_log_buffer`` on wake and pushes new entries onto every registered
# queue with ``put_nowait``; slow consumers drop on QueueFull instead of
# back-pressuring the producer. Mirrors the MavlinkIPCServer pattern.
_SUBSCRIBER_QUEUE_DEPTH = 256
_SSE_MAX_CLIENTS = int(os.environ.get("ADOS_SSE_MAX_CLIENTS", "16"))
_SSE_KEEPALIVE_S = 15.0
_subscribers: list[asyncio.Queue[dict | None]] = []
_subscribers_lock = asyncio.Lock()
_fanout_task: asyncio.Task | None = None
# Sentinel pushed onto a subscriber's queue when the subscriber must
# disconnect (slow-client drop). Distinct from ordinary dict entries.
_DROP_SENTINEL: dict | None = None


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
        # No loop bound yet (test harness, early boot). The next
        # fanout tick picks up via the keep-alive fallback.
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


async def _fanout_loop() -> None:
    """Single producer task: drain new entries, push to each subscriber.

    Runs while at least one subscriber is registered. Reads
    ``_log_buffer`` on each wake, emits every entry past the
    last-seen sequence number to every subscriber queue. A subscriber
    whose queue is full gets a ``_DROP_SENTINEL`` so its generator can
    close cleanly; the subscriber is unregistered on close. Filtering
    by ``level`` / ``service`` happens subscriber-side (per generator)
    so the producer stays cheap and shared across all clients.
    """
    last_seen_seq = 0
    while True:
        try:
            await asyncio.wait_for(_log_event.wait(), timeout=_SSE_KEEPALIVE_S)
        except asyncio.TimeoutError:
            # Periodic keep-alive: poke each subscriber with a sentinel
            # so their generator wakes and emits a heartbeat comment.
            async with _subscribers_lock:
                queues = list(_subscribers)
            for q in queues:
                try:
                    q.put_nowait({"__keepalive__": True})
                except asyncio.QueueFull:
                    pass
            continue
        _log_event.clear()
        tail = list(_log_buffer)
        async with _subscribers_lock:
            queues = list(_subscribers)
        if not queues:
            # No subscribers; bail. The wake-up still happened so the
            # next event will trip _log_event.set() and we loop again
            # — but exiting here means the producer task only runs
            # when there's work to do.
            if tail:
                last_seen_seq = tail[-1]["seq"]
            return
        new_entries = [e for e in tail if e["seq"] > last_seen_seq]
        if tail:
            last_seen_seq = tail[-1]["seq"]
        for entry in new_entries:
            for q in queues:
                try:
                    q.put_nowait(entry)
                except asyncio.QueueFull:
                    # Slow client. Push a drop sentinel and let the
                    # subscriber's generator close itself out so the
                    # producer never blocks.
                    try:
                        q.put_nowait(_DROP_SENTINEL)
                    except asyncio.QueueFull:
                        pass


async def _ensure_fanout_running() -> None:
    """Spawn the fanout task on demand. Idempotent."""
    global _fanout_task
    if _fanout_task is None or _fanout_task.done():
        _fanout_task = asyncio.create_task(_fanout_loop())


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
    EventSource API; backpressure is best-effort — a slow client is
    dropped rather than blocking the producer.

    Concurrent client cap (``_SSE_MAX_CLIENTS``) prevents a LAN
    attacker pre-pairing from pinning the board by opening hundreds
    of connections. Configurable via the ``ADOS_SSE_MAX_CLIENTS``
    environment variable.
    """
    # Bind the loop lazily on the first stream open so worker-thread
    # log writes can wake subscribers thread-safely.
    if _main_loop is None:
        bind_loop(asyncio.get_running_loop())

    async with _subscribers_lock:
        if len(_subscribers) >= _SSE_MAX_CLIENTS:
            raise HTTPException(
                status_code=503,
                detail=(
                    f"Too many SSE subscribers ({len(_subscribers)}/"
                    f"{_SSE_MAX_CLIENTS}). Try again later."
                ),
                headers={"Retry-After": "30"},
            )
        queue: asyncio.Queue[dict | None] = asyncio.Queue(
            maxsize=_SUBSCRIBER_QUEUE_DEPTH
        )
        _subscribers.append(queue)

    await _ensure_fanout_running()

    level_upper = level.upper() if level else None

    def _filter(entry: dict) -> bool:
        if level_upper and entry.get("level") != level_upper:
            return False
        if service and service not in entry.get("logger", ""):
            return False
        return True

    async def gen():
        try:
            # Initial snapshot of the most-recent 100 entries.
            snapshot = list(_log_buffer)[-100:]
            for e in snapshot:
                if _filter(e):
                    yield f"data: {json.dumps(e)}\n\n"
            while True:
                entry = await queue.get()
                if entry is _DROP_SENTINEL:
                    # Producer dropped us for being slow. Close cleanly
                    # so the client reconnects via EventSource auto-retry.
                    yield ": dropped slow client\n\n"
                    return
                if entry.get("__keepalive__"):
                    yield ": keep-alive\n\n"
                    continue
                if _filter(entry):
                    yield f"data: {json.dumps(entry)}\n\n"
        except asyncio.CancelledError:
            return
        finally:
            async with _subscribers_lock:
                try:
                    _subscribers.remove(queue)
                except ValueError:
                    pass

    return StreamingResponse(gen(), media_type="text/event-stream")
