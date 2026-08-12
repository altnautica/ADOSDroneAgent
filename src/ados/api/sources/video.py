"""Latest video telemetry, sourced from the durable logging store.

The latency route used to read ``lcd-latency.json`` straight off disk on every
request. The store's sidecar tailer already samples that same file into a
durable, time-aligned ``video.latency.*`` series plus the
``video.latency_source`` string event, so this helper reads the snapshot back
from the store instead — one sampler, a thin route, history for free.

Returns ``dict | None``. ``None`` means the store is unreachable or the producer
has not been running (no rows in the window), so the caller falls back to its
live read and the route degrades exactly as it did before, never to a 500. The
sidecar file keeps being written byte-identically, so the live fallback path is
unchanged.
"""

from __future__ import annotations

from typing import Any

from ados.api.telemetry_source import latest_metrics, query_rows

_LATENCY_METRICS = {
    "video.latency.glass_ms": "latency_ms",
    "video.latency.ewma_ms": "ewma_ms",
    "video.latency.samples": "samples",
}


def _metric_value(
    metrics: dict[str, dict[str, Any]] | None, name: str
) -> float | None:
    """The newest numeric value for ``name``, or ``None`` if absent."""
    if not metrics:
        return None
    row = metrics.get(name)
    if not isinstance(row, dict):
        return None
    value = row.get("value")
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return float(value)


async def _latest_event(kind: str, limit: int = 50) -> dict[str, Any] | None:
    """The newest events row whose ``kind`` matches, or ``None``.

    Filters the events table to the kind server-side via ``event_kind`` (the
    ``kind`` query param selects the table, not the event classifier), so the
    page is dense with the snapshot events rather than diluted by unrelated
    transitions. Re-checks the kind client-side so a store that ignores the
    filter cannot return the wrong event.
    """
    rows = await query_rows("events", limit, event_kind=kind)
    if not rows:
        return None
    for row in rows:  # newest-first
        if isinstance(row, dict) and row.get("kind") == kind:
            detail = row.get("detail")
            return detail if isinstance(detail, dict) else {}
    return None


async def latest_video_latency() -> dict[str, Any] | None:
    """Reconstruct the ``/video/latency`` route body from the store.

    Maps the ``video.latency.*`` metrics back to the route keys and reads the
    ``source`` off the ``video.latency_source`` event the tap produces, falling
    back to ``"sei"`` when that event is not in the window. Returns ``None`` when neither the glass-to-glass sample nor
    the sample count is present (the SEI probe is disabled or has produced
    nothing), so the route degrades to the same ``{latency_ms: None, source:
    "unavailable"}`` the live read returns.
    """
    metrics = await latest_metrics(set(_LATENCY_METRICS))
    glass = _metric_value(metrics, "video.latency.glass_ms")
    samples = _metric_value(metrics, "video.latency.samples")
    if glass is None and samples is None:
        return None
    ewma = _metric_value(metrics, "video.latency.ewma_ms")
    src_event = await _latest_event("video.latency_source")
    return {
        "latency_ms": glass,
        "ewma_ms": ewma,
        "samples": int(samples) if samples is not None else None,
        "source": src_event.get("source", "sei") if src_event else "sei",
    }


__all__ = ["latest_video_latency"]
