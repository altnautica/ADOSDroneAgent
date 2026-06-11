"""WFB link-status, history, and failover read-source helpers.

The ``/api/wfb`` family used to read its data straight off the ``/run/ados``
sidecar files the native radio writes. The radio (``ados-radio`` / the GS
``ados-groundlink``) now also ships the same full body to the durable logging
store as a ``link.wfb_status`` event, and the per-heartbeat link samples as
``link.*`` metrics, so these helpers read that back instead. The route reads the
store first and falls back to the sidecar file, so losing the store degrades to
the old behavior, never to a 500.

The route-computed legs that are NOT producer fields (``frequency_mhz`` /
``bandwidth_mhz`` re-derived from the channel, the ``bitrate_mbps`` shim, and the
LIVE ``regulatory_domain``) are re-applied here over the same base block the live
read uses, so the store-derived response is byte-identical to the live one.
"""

from __future__ import annotations

from typing import Any

import httpx

from ados.api.telemetry_source import _get_client, query_rows

# Beyond this age (microseconds) the most-recent status event is treated as
# stale, mirroring the live read's ``mtime > 10 s`` flip. The store carries the
# producer's emit timestamp on the row, so the staleness check uses the event's
# ``ts_us`` rather than a file mtime.
_STALE_AGE_US = 10_000_000

# The failover states the route validates against. ``failed`` is tolerated by the
# route but never produced; keep it so the helper and the route validate
# identically (an unknown value yields ``None`` so the route falls back).
_FAILOVER_STATES = {"local", "cloud_relay", "failed"}

# The aggregate metrics that compose a history sample, mapped to their sample-row
# key. ``agg=last`` per bucket picks the reading at that instant.
_HIST_KEY = {
    "link.rssi_dbm": "rssi_dbm",
    "link.snr_db": "snr_db",
    "link.loss_percent": "loss_percent",
    "link.bitrate_kbps": "bitrate_kbps",
}


async def latest_wfb_status() -> tuple[dict[str, Any], int] | None:
    """Most-recent full wfb-status snapshot + its emit timestamp, or ``None``.

    Returns ``(detail, ts_us)`` where ``detail`` is the full sidecar body the
    radio shipped and ``ts_us`` is the row's emit timestamp (used for the
    staleness check). ``None`` when the store is unreachable or has captured no
    ``link.wfb_status`` event yet, so the caller falls back to the sidecar file.
    """
    rows = await query_rows("events", 1, event_kind="link.wfb_status")
    if not rows:
        return None
    row = rows[0]
    if not isinstance(row, dict):
        return None
    detail = row.get("detail")
    if not isinstance(detail, dict) or not detail:
        return None
    ts_us = row.get("ts_us")
    ts_us = int(ts_us) if isinstance(ts_us, (int, float)) else 0
    return detail, ts_us


def derive_wfb_status(detail: dict[str, Any], ts_us: int, wfb_cfg: object) -> dict:
    """Map a stored status body back to the exact ``/api/wfb`` shape.

    Re-applies the route's computed/live legs so the result matches the live
    ``_build_status_from_stats_file`` for the same underlying data: the
    config-seeded base, the body merged over it, the LIVE ``regulatory_domain``
    override, the frequency/bandwidth re-derivation, and the ``bitrate_mbps``
    shim. Staleness uses the event age (``now - ts_us``) in place of the live
    read's file mtime, so an event older than the threshold flips ``state`` to
    ``"stale"`` exactly as the file path does.
    """
    import time as _time

    # Import here (not at module load) to avoid a route<->source import cycle:
    # the route module imports this source's helpers, and these legs live on the
    # route so the live and store paths share one implementation.
    from ados.api.routes.wfb import _base_block, _finalize_status

    merged = _base_block(wfb_cfg)
    # regulatory_domain stays the LIVE value `_base_block` just set from one
    # `iw reg get`: the stored body carries `reg_domain` (not `regulatory_domain`),
    # so this update never overwrites it — no second domain read is needed, matching
    # the live `_build_status_from_stats_file` path which also reuses the base value.
    merged.update(detail)
    # Event-age staleness, mirroring the file-mtime flip on the live path.
    age_us = _time.time() * 1_000_000 - ts_us
    if ts_us > 0 and age_us > _STALE_AGE_US:
        merged["state"] = "stale"
    return _finalize_status(merged)


async def latest_wfb_history(seconds: int) -> dict[str, Any] | None:
    """Durable link-quality history reshaped into the route's sample list.

    Aggregates the ``link.*`` metrics into time buckets via ``/v1/aggregate`` and
    groups them by bucket into ``{samples: [{timestamp, rssi_dbm, snr_db,
    loss_percent, bitrate_kbps}], count}``. ``None`` when the store is
    unreachable, so the route falls back to the native empty history.
    """
    seconds = min(max(seconds, 1), 300)
    client = _get_client()
    params = [
        ("since", f"-{seconds}s"),
        ("bucket", "auto"),
        ("agg", "last"),
    ]
    params.extend(("metric", name) for name in _HIST_KEY)
    try:
        resp = await client.get("/v1/aggregate", params=params)
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

    buckets = body.get("data") if isinstance(body, dict) else None
    if not isinstance(buckets, list):
        return None

    # Group the per-metric buckets into one sample per bucket instant.
    by_ts: dict[int, dict[str, Any]] = {}
    for b in buckets:
        if not isinstance(b, dict):
            continue
        metric = b.get("metric")
        key = _HIST_KEY.get(metric) if isinstance(metric, str) else None
        bucket_us = b.get("bucket_us")
        if key is None or not isinstance(bucket_us, (int, float)):
            continue
        slot = by_ts.setdefault(int(bucket_us), {})
        slot[key] = b.get("value")

    samples = [
        {"timestamp": _iso_from_us(ts), **vals} for ts, vals in sorted(by_ts.items())
    ]
    return {"samples": samples, "count": len(samples)}


async def latest_wfb_failover() -> str | None:
    """Most-recent local-bind to cloud-relay failover state from the store.

    Reads the newest ``wfb.pair.failover`` event the supervisor emitted and
    returns its ``detail.state``, validated to the same set the live route
    accepts. ``None`` when the store is unreachable, holds no such event, or
    carries an unrecognized value, so the route falls back to the sidecar. A
    losable store degrades to the old behavior, never to a 500.
    """
    rows = await query_rows("events", 1, event_kind="wfb.pair.failover")
    if not rows:
        return None
    row = rows[0]
    if not isinstance(row, dict):
        return None
    detail = row.get("detail")
    if not isinstance(detail, dict):
        return None
    state = detail.get("state")
    if isinstance(state, str) and state in _FAILOVER_STATES:
        return state
    return None


def _iso_from_us(ts_us: int) -> str:
    """Render a microsecond-epoch timestamp as an ISO-8601 UTC string."""
    import datetime as _dt

    return (
        _dt.datetime.fromtimestamp(ts_us / 1_000_000, tz=_dt.timezone.utc)
        .isoformat()
        .replace("+00:00", "Z")
    )


__all__ = [
    "latest_wfb_status",
    "derive_wfb_status",
    "latest_wfb_history",
    "latest_wfb_failover",
]
