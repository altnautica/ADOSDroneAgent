"""Latest per-service memory, sourced from the durable logging store.

The ``/api/services`` route attaches a ``memory_mb`` to every service entry by
running a process-local PSS scan over ``/proc`` on demand (one scan groups every
``ados-*.service`` unit's processes by cgroup and sums their PSS). The supervisor
runs that same grouped sum continuously, as root, and ships one
``service.memory_pss_bytes`` metric per unit to the store, so this helper reads
the latest per-unit value back from the durable series instead of scanning on
every request.

The helper returns ``dict[unit_name, mb] | None``. ``None`` means the store is
unreachable or the sampler has produced no rows in the window (e.g. on a fresh
boot before the first sample), so the route falls back to its live scan and
degrades exactly as it did before, never to a 500. Bytes are converted to MiB
the same way the live path does (``round(bytes / 1024 / 1024, 1)``), so the two
paths report identical ``memory_mb`` values for the same underlying PSS.

Every unit shares the one ``service.memory_pss_bytes`` metric name and is
distinguished by its ``unit`` tag, so this reads the raw metric page directly
(``query_rows``) and folds it newest-wins per unit, rather than going through
``latest_metrics`` which collapses to a single newest row per metric *name* (it
would keep only one unit). Same shared client, same timeout, same gap tolerance.

Scope note: ``/api/services`` reads the store first through this helper. The
``/api/status`` services list and the cloud-heartbeat payload still run the live
``services_memory_mb`` scan directly (the values are identical, and the root API
process reads PSS fine). Routing those two surfaces through this helper is a
consistency follow-up best done alongside the native status surface, since both
build their services list synchronously today.
"""

from __future__ import annotations

from typing import Any

from ados.api.telemetry_source import query_rows

# The dotted metric the supervisor's per-service sampler emits, one row per unit
# per sample, tagged with the owning unit name.
_METRIC_MEMORY_PSS_BYTES = "service.memory_pss_bytes"
_TAG_UNIT = "unit"

# Match the live path's KiB→MiB rounding (it divides KiB by 1024 to one decimal);
# the sampler ships bytes, so divide by 1024² and round to the same one decimal.
_BYTES_PER_MB = 1024 * 1024

# Enough recent metric rows to cover one full sweep of the fleet. The sampler
# ships one row per unit per ~5 s tick across ~a dozen-plus units, so a single
# tick's worth of rows fits comfortably inside this page and the newest value of
# every unit appears at least once.
_METRIC_LIMIT = 200


def _bytes_to_mb(value: Any) -> float | None:
    """MiB (1 decimal) from a byte reading, or ``None`` when non-numeric.

    ``bool`` is excluded explicitly: in Python it is an ``int`` subclass and a
    boolean is never a byte measurement.
    """
    if isinstance(value, bool):
        return None
    if not isinstance(value, (int, float)):
        return None
    return round(float(value) / _BYTES_PER_MB, 1)


async def latest_service_memory() -> dict[str, float] | None:
    """Newest per-unit PSS (MiB) from the store, keyed by ``ados-*.service``.

    Walks the most-recent ``service.memory_pss_bytes`` rows (newest-first, one
    per unit per sample) and keeps the first byte value seen for each ``unit``
    tag, then converts bytes→MiB. Returns ``None`` when the store is unreachable
    or no row carries a usable unit tag, so the caller falls back to the live
    ``/proc`` scan.
    """
    rows = await query_rows("metrics", _METRIC_LIMIT)
    if rows is None:
        return None
    out: dict[str, float] = {}
    for entry in rows:  # newest-first
        if not isinstance(entry, dict):
            continue
        if entry.get("metric") != _METRIC_MEMORY_PSS_BYTES:
            continue
        tags = entry.get("tags")
        unit = tags.get(_TAG_UNIT) if isinstance(tags, dict) else None
        if not isinstance(unit, str) or not unit:
            continue
        if unit in out:  # already has its newest value
            continue
        mb = _bytes_to_mb(entry.get("value"))
        if mb is None:
            continue
        out[unit] = mb
    return out or None


__all__ = [
    "latest_service_memory",
]
