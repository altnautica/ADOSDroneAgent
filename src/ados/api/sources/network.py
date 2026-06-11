"""Ground-station uplink + modem-usage read-source helpers.

The aggregate ``GET /network`` view and the ``GET /network/modem`` view used to
compose their active-uplink + cumulative-data-cap legs from an in-process
``UplinkRouter`` singleton and a live ``/sys`` counter read. The health loop now
runs in the native ``ados-net`` daemon, so the FastAPI-process singleton never
ticks (its ``active_uplink`` is dead-on-read) and the live counter read may not
even see the modem iface. The daemon writes the real values to its sidecars
(``/run/ados/uplink-active``, ``/var/lib/ados/modem-usage.json``) AND ships the
same bodies to the durable store as ``net.uplink_active`` / ``net.modem_usage``
events, so these helpers read that truth back.

Both routes read the store first and fall back to the existing live read, so
losing the store degrades to the old behavior, never to a 500. Only the
store-backable legs route through here; every other leg of those views (the AP /
wifi-client / ethernet / share probes, the live modem status, the priority
config file) stays live and is untouched.
"""

from __future__ import annotations

from typing import Any

from ados.api.telemetry_source import query_rows


async def latest_uplink_active() -> dict[str, Any] | None:
    """Most-recent active-uplink snapshot the router daemon shipped, or ``None``.

    Returns the stored ``net.uplink_active`` detail body
    (``{active_uplink, internet_reachable, timestamp_ms, data_cap_state}``).
    ``active_uplink`` is ``None`` in the body when the router has no viable
    uplink (the daemon emits the same keys with a null uplink on unlink), so a
    store-first reader learns "no uplink" without a separate file probe.

    ``None`` when the store is unreachable or has captured no such event yet, so
    the caller falls back to the live in-process router view. A losable store
    degrades to the old behavior, never to a 500.
    """
    rows = await query_rows("events", 1, event_kind="net.uplink_active")
    if not rows:
        return None
    row = rows[0]
    if not isinstance(row, dict):
        return None
    detail = row.get("detail")
    if not isinstance(detail, dict) or not detail:
        return None
    return detail


async def latest_modem_usage() -> dict[str, Any] | None:
    """Most-recent cumulative data-cap usage block the daemon shipped, or ``None``.

    Returns the stored ``net.modem_usage`` detail body
    (``{data_used_mb, cap_mb, percent, state, window_reset_at,
    last_reset_month}``) — the exact ``DataCapTracker.get_usage()`` snapshot the
    modem view serves. ``None`` when the store is unreachable or holds no such
    event, so the caller falls back to the live ``data_usage()``-derived figures.
    """
    rows = await query_rows("events", 1, event_kind="net.modem_usage")
    if not rows:
        return None
    row = rows[0]
    if not isinstance(row, dict):
        return None
    detail = row.get("detail")
    if not isinstance(detail, dict) or not detail:
        return None
    return detail


__all__ = [
    "latest_uplink_active",
    "latest_modem_usage",
]
