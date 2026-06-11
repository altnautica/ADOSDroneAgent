"""Latest hardware telemetry, sourced from the durable logging store.

The diagnostics/status/system routes used to read CPU/memory/disk/temperature
straight from ``psutil`` on every request. The Rust hardware collector already
samples all of that continuously into the store, so these helpers read the same
readings back from the store's most-recent hardware snapshots instead — one
sampler, thin routes.

A store snapshot is sparse per tick (each signal class fires on its own cadence),
so a single latest row does not carry every field. The merge here folds the most
recent handful of snapshots into one signal map, newest value winning, so a full
picture is assembled from the last ~couple of seconds.

When the store is unreachable, or has not yet captured the essential fields, the
helper returns ``None`` and the caller falls back to a live ``psutil`` read.
Losing the store degrades to the old behavior, never to a 500.
"""

from __future__ import annotations

from typing import Any

import httpx

from ados.core.paths import LOGD_QUERY_SOCK

# The store's query API over the trusted local Unix socket. The host portion is a
# placeholder httpx requires; the UDS transport routes to the socket regardless.
_UPSTREAM_BASE = "http://logd"

# Connect fast so a missing store degrades the route at once; the read is a single
# small bounded page.
_TIMEOUT = httpx.Timeout(connect=1.0, read=2.0, write=1.0, pool=1.0)

# How many recent hw snapshots to merge. At the collector's 100 ms base tick this
# is ~2 s of history — comfortably more than the slowest class cadence (1 s), so
# every field appears at least once in the window.
_MERGE_ROWS = 20

_BYTES_PER_MB = 1024 * 1024
_BYTES_PER_GB = 1024 * 1024 * 1024

_client: httpx.AsyncClient | None = None


def _get_client() -> httpx.AsyncClient:
    global _client
    if _client is None:
        transport = httpx.AsyncHTTPTransport(uds=str(LOGD_QUERY_SOCK))
        _client = httpx.AsyncClient(
            base_url=_UPSTREAM_BASE, transport=transport, timeout=_TIMEOUT
        )
    return _client


async def aclose() -> None:
    """Close the shared upstream client. Called on app shutdown."""
    global _client
    if _client is not None:
        await _client.aclose()
        _client = None


async def latest_hw_signals() -> dict[str, Any] | None:
    """Merge the most recent hw snapshots into one signal map (newest wins).

    Returns ``None`` when the store is unreachable or has no hardware rows, so the
    caller falls back to a live read.
    """
    client = _get_client()
    try:
        resp = await client.get(
            "/v1/query", params={"kind": "hw", "limit": _MERGE_ROWS}
        )
    except (httpx.ConnectError, httpx.ConnectTimeout, FileNotFoundError, OSError):
        return None
    except httpx.HTTPError:
        return None
    if resp.status_code >= 400:
        return None
    try:
        body = resp.json()
    except ValueError:
        return None

    rows = body.get("data") if isinstance(body, dict) else None
    if not isinstance(rows, list) or not rows:
        return None

    # Rows are newest-first; the first time a signal key is seen is its newest
    # value, so a plain "insert if absent" merge keeps the freshest reading.
    merged: dict[str, Any] = {}
    for row in rows:
        sig = row.get("signals") if isinstance(row, dict) else None
        if not isinstance(sig, dict):
            continue
        for key, value in sig.items():
            if key not in merged:
                merged[key] = value
    return merged or None


async def query_rows(
    kind: str, limit: int, **params: Any
) -> list[dict[str, Any]] | None:
    """Page the store's /v1/query for one row kind. None on any gap.

    The single try/except ladder every domain reuses, so a missing store
    degrades each route to its live fallback rather than to a 500.
    """
    client = _get_client()
    q = {"kind": kind, "limit": limit, **params}
    try:
        resp = await client.get("/v1/query", params=q)
    except (
        httpx.ConnectError,
        httpx.ConnectTimeout,
        FileNotFoundError,
        OSError,
        httpx.HTTPError,
    ):
        return None
    if resp.status_code >= 400:
        return None
    try:
        body = resp.json()
    except ValueError:
        return None
    rows = body.get("data") if isinstance(body, dict) else None
    return rows if isinstance(rows, list) else None


async def latest_metrics(
    names: set[str], limit: int = 200
) -> dict[str, dict[str, Any]] | None:
    """Newest value (+ tags + ts) per named metric from recent ``metrics`` rows.

    Returns a {metric_name: {"value": float, "tags": {...}, "ts_us": int}} map,
    newest-wins. ``None`` when the store is unreachable. Names not seen in the
    window are simply absent from the map (caller decides if that is a gap).
    """
    rows = await query_rows("metrics", limit)
    if rows is None:
        return None
    out: dict[str, dict[str, Any]] = {}
    for row in rows:  # newest-first
        if not isinstance(row, dict):
            continue
        m = row.get("metric")
        if m in names and m not in out:
            out[m] = {
                "value": row.get("value"),
                "tags": row.get("tags") or {},
                "ts_us": row.get("ts_us"),
            }
    return out or None


def _num(signals: dict[str, Any], key: str) -> float | None:
    """Return a numeric signal value, or ``None`` if absent / non-numeric.

    ``bool`` is excluded explicitly: in Python it is an ``int`` subclass, and a
    boolean signal is never a measurement.
    """
    value = signals.get(key)
    if isinstance(value, bool):
        return None
    if isinstance(value, (int, float)):
        return float(value)
    return None


def derive_resources(signals: dict[str, Any]) -> dict[str, Any] | None:
    """Map merged hw signals to the canonical resource fields the routes expose.

    Returns ``None`` when an essential field is missing, so the route falls back
    to a complete live ``psutil`` read rather than serving a half-populated reply.
    The essential set is memory total + available, aggregate CPU utilization, and
    filesystem total + used — the spine of every resource readout.
    """
    total = _num(signals, "mem.total_bytes")
    avail = _num(signals, "mem.avail_bytes")
    cpu = _num(signals, "cpu.util.all")
    disk_total = _num(signals, "disk.fs_total_bytes")
    disk_used = _num(signals, "disk.fs_used_bytes")
    if None in (total, avail, cpu, disk_total, disk_used):
        return None

    used = max(total - avail, 0.0)
    swap_total = _num(signals, "mem.swap_total_bytes") or 0.0
    swap_free = _num(signals, "mem.swap_free_bytes") or 0.0
    swap_used = max(swap_total - swap_free, 0.0)
    cache = _num(signals, "mem.cache_bytes") or 0.0

    # Per-sensor temperature map from the thermal.<sensor>_c signals. The primary
    # is a duplicate of the first zone and is surfaced separately, so skip it; the
    # hwmon entries keep their dotted sub-name (e.g. "hwmon.rpi_volt_temp1").
    temps: dict[str, float] = {}
    for key, value in signals.items():
        if not (key.startswith("thermal.") and key.endswith("_c")):
            continue
        if key == "thermal.primary_c":
            continue
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            continue
        temps[key[len("thermal.") : -len("_c")]] = float(value)

    primary = _num(signals, "thermal.primary_c")
    load = [
        v
        for v in (
            _num(signals, "sched.loadavg_1"),
            _num(signals, "sched.loadavg_5"),
            _num(signals, "sched.loadavg_15"),
        )
        if v is not None
    ]

    return {
        "cpu_percent": round(cpu, 1),
        "memory_total_mb": round(total / _BYTES_PER_MB),
        "memory_used_mb": round(used / _BYTES_PER_MB),
        "memory_available_mb": round(avail / _BYTES_PER_MB),
        "memory_cache_mb": round(cache / _BYTES_PER_MB),
        "memory_percent": round(used / total * 100, 1) if total else 0.0,
        "swap_total_mb": round(swap_total / _BYTES_PER_MB),
        "swap_used_mb": round(swap_used / _BYTES_PER_MB),
        "swap_percent": round(swap_used / swap_total * 100, 1) if swap_total else 0.0,
        "disk_total_gb": round(disk_total / _BYTES_PER_GB, 1),
        "disk_used_gb": round(disk_used / _BYTES_PER_GB, 1),
        "disk_percent": round(disk_used / disk_total * 100, 1) if disk_total else 0.0,
        "temperature": primary,
        "temperatures": temps,
        "load_avg": load,
    }


__all__ = [
    "latest_hw_signals",
    "derive_resources",
    "aclose",
    "query_rows",
    "latest_metrics",
]
