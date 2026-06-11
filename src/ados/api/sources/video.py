"""Latest video telemetry, sourced from the durable logging store.

The video routes used to read process-local pipeline state and the runtime
sidecar JSON files (``air-pipeline.json`` / ``lcd-latency.json``) straight off
disk on every request. The store's sidecar tailer already samples those same
files into a durable, time-aligned ``video.air.*`` / ``video.latency.*`` series
plus the ``video.air_state`` / ``video.latency_source`` string events, so these
helpers read the snapshot back from the store instead — one sampler, thin
routes, history for free.

Each helper returns ``dict | None``. ``None`` means the store is unreachable or
the producer has not been running (no rows in the window), so the caller falls
back to its live read and the route degrades exactly as it did before, never to
a 500. The sidecar files keep being written byte-identically, so the live
fallback path is unchanged.

Two fields the air-pipeline route returns carry no store-portable value: the
``started_at`` / ``last_state_change_at`` / ``last_buffer_at`` floats are
``time.monotonic()`` references with no cross-process meaning, so the store
cannot reconstruct them. The helper sets them to ``None``; the route merges the
real value from the live file when it is present.
"""

from __future__ import annotations

from typing import Any

from ados.api.telemetry_source import latest_metrics, query_rows

# The numeric air-pipeline metric series, mapped back to the JSON keys the route
# returns. Each entry is (metric name in the store, route key, caster).
_AIR_INT_METRICS = {
    "video.air.sei_injected_count": "sei_injected_count",
    "video.air.udp_bytes_out": "udp_bytes_out",
    "video.air.restart_count": "restart_count",
    "video.air.tx_silent_kicks": "tx_silent_kicks",
    "video.air.bus_errors": "bus_errors",
    "video.air.updated_at_ms": "updated_at_ms",
}
_AIR_FLOAT_METRICS = {
    "video.air.encoder_fps": ("encoder_fps", 2),
    "video.air.encoded_kbps": ("encoded_kbps", 1),
}
_AIR_BOOL_METRICS = {
    "video.air.encoder_hw_accel": "encoder_hw_accel",
    "video.air.cloud_branch_open": "cloud_branch_open",
}
_AIR_METRIC_NAMES = (
    set(_AIR_INT_METRICS)
    | {k for k in _AIR_FLOAT_METRICS}
    | set(_AIR_BOOL_METRICS)
)

# The three monotonic-clock floats the store cannot carry; the route fills these
# from the live file when present, else they stay None.
_AIR_LIVE_ONLY_FLOATS = ("started_at", "last_state_change_at", "last_buffer_at")

_LATENCY_METRICS = {
    "video.latency.glass_ms": "latency_ms",
    "video.latency.ewma_ms": "ewma_ms",
    "video.latency.pipeline_ms": "pipeline_latency_ms",
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


async def latest_air_pipeline() -> dict[str, Any] | None:
    """Reconstruct the ``air-pipeline.json`` route body from the store.

    Maps each ``video.air.*`` metric back to its ``AirPipelineStats.to_dict()``
    key (re-casting integer counters, float gauges, and the two bool flags) and
    pulls the three strings from the latest ``video.air_state`` event. The three
    monotonic-clock floats are set to ``None`` (the store does not carry them);
    the route merges the live value when the file is present.

    Returns ``None`` when neither the metric series nor the state event is in the
    window (the air pipeline is not running), so the route falls through to the
    live file read and its 204 / 503 contract is preserved.
    """
    metrics = await latest_metrics(_AIR_METRIC_NAMES)
    air = await _latest_event("video.air_state")
    if metrics is None and air is None:
        return None

    out: dict[str, Any] = {}
    # Strings from the state event (empty defaults match the dataclass).
    out["camera_source"] = (air or {}).get("camera_source") or ""
    out["encoder_name"] = (air or {}).get("encoder_name") or ""
    out["pipeline_state"] = (air or {}).get("pipeline_state") or "idle"

    for name, key in _AIR_INT_METRICS.items():
        value = _metric_value(metrics, name)
        out[key] = int(value) if value is not None else 0
    for name, (key, ndigits) in _AIR_FLOAT_METRICS.items():
        value = _metric_value(metrics, name)
        out[key] = round(value, ndigits) if value is not None else 0.0
    for name, key in _AIR_BOOL_METRICS.items():
        value = _metric_value(metrics, name)
        out[key] = bool(value >= 0.5) if value is not None else False

    # The monotonic-clock floats carry no cross-process meaning; the route fills
    # them from the live file when it can, else they stay None.
    for key in _AIR_LIVE_ONLY_FLOATS:
        out[key] = None
    return out


async def latest_video_latency() -> dict[str, Any] | None:
    """Reconstruct the ``/video/latency`` route body from the store.

    Maps the ``video.latency.*`` metrics back to the route keys and reads the
    ``source`` off the ``video.latency_source`` event the tap produces (same
    pattern as ``video.air_state``), falling back to ``"sei"`` when that event is
    not in the window. Returns ``None`` when neither the glass-to-glass sample nor
    the sample count is present (the SEI probe is disabled or has produced
    nothing), so the route degrades to the same ``{latency_ms: None, source:
    "unavailable"}`` the live read returns.
    """
    metrics = await latest_metrics(set(_LATENCY_METRICS))
    glass = _metric_value(metrics, "video.latency.glass_ms")
    samples = _metric_value(metrics, "video.latency.samples")
    if glass is None and samples is None:
        return None
    pipeline = _metric_value(metrics, "video.latency.pipeline_ms")
    ewma = _metric_value(metrics, "video.latency.ewma_ms")
    src_event = await _latest_event("video.latency_source")
    return {
        "latency_ms": glass,
        "ewma_ms": ewma,
        "pipeline_latency_ms": pipeline,
        "samples": int(samples) if samples is not None else None,
        "source": src_event.get("source", "sei") if src_event else "sei",
    }


__all__ = [
    "latest_air_pipeline",
    "latest_video_latency",
]
