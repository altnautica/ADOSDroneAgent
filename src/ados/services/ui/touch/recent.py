"""Recent-touch ring buffer consumed by the GCS Display sub-view.

The touch bridge publishes :class:`TouchGesture` events on a fanout
bus when the operator interacts with the panel. The Display sub-view
on the GCS wants a tail of those events so a remote operator can see
which corner of the LCD just got tapped and what kind of gesture it
was. The ring buffer here is a single deque of compact dicts; the
``/api/v1/display/touches`` route returns a slice of it.

Maintained as a process-wide singleton fed by a background consumer
that the OLED service spawns once during startup. The route reads it
lock-free; threading.Lock around the deque mutations is enough because
the bus consumer runs on the asyncio loop and the route handler runs
on FastAPI's executor — no concurrent appends, just one writer + many
readers.
"""

from __future__ import annotations

import threading
import time
from collections import deque
from typing import Any

# Keep the last 32 events. Matches the Display sub-view's request
# contract so a single GET returns a useful tail without paginating.
_RING_MAX = 32

_lock = threading.Lock()
_ring: deque[dict[str, Any]] = deque(maxlen=_RING_MAX)


def record_touch(
    *,
    kind: str,
    x: int,
    y: int,
    page: str | None,
    timestamp_ms: int | None = None,
) -> None:
    """Push a touch event onto the ring buffer.

    Called from the OLED service's gesture-consumer task right after
    the gesture is dispatched to a page. The tuple captured is
    deliberately small — the GCS only needs the kind, the LCD-pixel
    coordinates, and the active page id. We do NOT capture the full
    gesture sample list; a 32-deep ring of those would balloon the
    memory footprint for no operator-visible benefit.
    """
    t_ms = timestamp_ms if timestamp_ms is not None else int(time.time() * 1000)
    event = {
        "t": int(t_ms),
        "x": int(x),
        "y": int(y),
        "page": page or "",
        "kind": str(kind),
    }
    with _lock:
        _ring.append(event)


def recent_touches(since_ms: int = 0) -> list[dict[str, Any]]:
    """Return events with ``t > since_ms``, oldest-first.

    The default ``since_ms=0`` returns the full ring (up to 32 events).
    Callers polling at 1 Hz pass the last seen ``t`` to get only the
    new events; the route handler clips to that tail before
    serializing the response.
    """
    with _lock:
        snapshot = list(_ring)
    if since_ms <= 0:
        return snapshot
    return [e for e in snapshot if e["t"] > since_ms]


def last_touch() -> dict[str, Any] | None:
    """Return the most recent event, or ``None`` when the ring is empty.

    Used by the heartbeat enricher so the GCS card can show
    ``lcdLastTouchAt`` and ``lcdLastGesture`` between snapshot polls.
    """
    with _lock:
        if not _ring:
            return None
        return dict(_ring[-1])


def clear() -> None:
    """Wipe the ring. Used by tests + the OLED service on shutdown."""
    with _lock:
        _ring.clear()
