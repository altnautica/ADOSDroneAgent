"""The baseline observability routes (logs, hw summary/snapshot, service events)."""

from __future__ import annotations

from ..routes import FieldSpec, Locator, RouteSpec


def routes() -> list[RouteSpec]:
    """The baseline route set, in report order."""
    return [
        _log_route(),
        _hw_summary_route(),
        _hw_snapshot_route(),
        _service_events_route(),
        _service_memory_route(),
    ]


def _log_route() -> RouteSpec:
    """The durable log route, the canonical dual-run against the legacy handler.

    The legacy entry shape is ``{ seq, timestamp, level, logger, message }``;
    the store row is ``{ id, ts_us, source, level, target, msg, ... }``. Every
    legacy field maps onto a store column the store already serves, so the store
    is a strict superset (it additionally carries ``session``, ``fields`` and the
    redaction flag).
    """
    return RouteSpec(
        name="logs",
        kind="logs",
        logd_params={"kind": "logs", "limit": 50},
        legacy_path="/api/logs",
        legacy_entries_key="entries",
        observability_path="/api/v2/observability/logs",
        fields=[
            FieldSpec(
                field="id",
                locator=Locator.ROW_KEY,
                classification="history",
                producer="any",
                legacy_field="seq",
            ),
            FieldSpec(
                field="ts_us",
                locator=Locator.ROW_KEY,
                classification="history",
                producer="any",
                legacy_field="timestamp",
            ),
            FieldSpec(
                field="level",
                locator=Locator.ROW_KEY,
                classification="history",
                producer="any",
                legacy_field="level",
            ),
            FieldSpec(
                field="source",
                locator=Locator.ROW_KEY,
                classification="history",
                producer="any",
                legacy_field="logger",
            ),
            FieldSpec(
                field="msg",
                locator=Locator.ROW_KEY,
                classification="history",
                producer="any",
                legacy_field="message",
            ),
        ],
    )


def _hw_summary_route() -> RouteSpec:
    """Headline hardware summary metrics (live telemetry, store-only)."""
    return RouteSpec(
        name="hw-summary",
        kind="metrics",
        logd_params={"kind": "metrics", "limit": 200},
        observability_path="/api/v2/observability/metrics",
        fields=[
            FieldSpec(
                field="cpu.utilization_pct",
                locator=Locator.METRIC,
                classification="live",
                producer="ados-logd",
            ),
            FieldSpec(
                field="mem.available_pct",
                locator=Locator.METRIC,
                classification="live",
                producer="ados-logd",
            ),
            FieldSpec(
                field="disk.used_pct",
                locator=Locator.METRIC,
                classification="live",
                producer="ados-logd",
            ),
            FieldSpec(
                field="thermal.primary_c",
                locator=Locator.METRIC,
                classification="live",
                producer="ados-logd",
            ),
        ],
    )


def _hw_snapshot_route() -> RouteSpec:
    """Hardware-snapshot signals the resource routes read back (store-only).

    The diagnostics/status/system routes derive their CPU/memory/disk/temperature
    /load readouts from the most-recent hw snapshots, so the store must serve each
    backing signal in the ``hw`` row's open ``signals`` map. These are the
    net-new fields the collector grew to fully cover those routes (memory cache,
    swap total, filesystem bytes, load average) plus the spine it already carried.
    """
    names = [
        "mem.total_bytes",
        "mem.avail_bytes",
        "mem.cache_bytes",
        "mem.swap_total_bytes",
        "disk.fs_total_bytes",
        "disk.fs_used_bytes",
        "sched.loadavg_1",
        "thermal.primary_c",
        "cpu.util.all",
    ]
    return RouteSpec(
        name="hw-snapshot",
        kind="hw",
        logd_params={"kind": "hw", "limit": 200},
        observability_path="/api/v2/observability/hw",
        fields=[
            FieldSpec(
                field=name,
                locator=Locator.SIGNAL,
                classification="live",
                producer="ados-logd",
            )
            for name in names
        ],
    )


def _service_memory_route() -> RouteSpec:
    """Per-service memory the services route reads back (store-only).

    The supervisor samples each ``ados-*.service`` unit's grouped PSS from
    ``/proc`` continuously and ships one ``service.memory_pss_bytes`` metric per
    unit, tagged with the owning unit, so the services route serves per-service
    memory from history instead of scanning on every request. Every unit shares
    the one metric name and is distinguished by its ``unit`` tag, so the field
    here is the metric itself; absence of any row means the producer is not
    running (the route then falls back to its live scan).
    """
    return RouteSpec(
        name="service-memory",
        kind="metrics",
        logd_params={"kind": "metrics", "limit": 200},
        observability_path="/api/v2/observability/metrics",
        fields=[
            FieldSpec(
                field="service.memory_pss_bytes",
                locator=Locator.METRIC,
                classification="live",
                producer="ados-supervisor",
            ),
        ],
    )


def _service_events_route() -> RouteSpec:
    """Supervisor service-transition events (live events, store-only)."""
    return RouteSpec(
        name="service-events",
        kind="events",
        logd_params={"kind": "events", "limit": 200},
        observability_path="/api/v2/observability/events",
        row_match={"kind": "service.transition"},
        fields=[
            FieldSpec(
                field="from_state",
                locator=Locator.DETAIL_KEY,
                classification="live",
                producer="ados-supervisor",
            ),
            FieldSpec(
                field="to_state",
                locator=Locator.DETAIL_KEY,
                classification="live",
                producer="ados-supervisor",
            ),
            FieldSpec(
                field="reason",
                locator=Locator.DETAIL_KEY,
                classification="live",
                producer="ados-supervisor",
            ),
        ],
    )
