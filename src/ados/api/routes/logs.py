"""Log viewing routes."""

from __future__ import annotations

import logging
from collections import deque

from fastapi import APIRouter, Query

router = APIRouter()

# In-memory log buffer
_log_buffer: deque[dict] = deque(maxlen=1000)


class BufferHandler(logging.Handler):
    """Captures log records into an in-memory ring buffer."""

    def emit(self, record: logging.LogRecord) -> None:
        _log_buffer.append({
            "timestamp": self.format(record) if not record.created else record.created,
            "level": record.levelname,
            "logger": record.name,
            "message": record.getMessage(),
        })


def install_log_buffer() -> None:
    """Install the buffer handler on the root logger."""
    handler = BufferHandler()
    handler.setLevel(logging.DEBUG)
    logging.getLogger().addHandler(handler)


@router.get("/logs")
async def get_logs(
    level: str | None = Query(None),
    service: str | None = Query(None),
    limit: int = Query(50, ge=1, le=500),
    offset: int = Query(0, ge=0),
):
    """Recent log entries with optional filtering."""
    entries = list(_log_buffer)

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
    }
